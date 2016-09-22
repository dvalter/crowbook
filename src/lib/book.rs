use error::{Error,Result, Source};
use cleaner::{Cleaner, French, Off, Default};
use bookoptions::BookOptions;
use parser::Parser;
use token::Token;
use epub::EpubRenderer;
use html_single::HtmlSingleRenderer;
use html_dir::HtmlDirRenderer;
use latex::LatexRenderer;
use odt::OdtRenderer;
use templates::{epub, html, epub3, latex, html_dir, highlight};
use number::Number;
use resource_handler::ResourceHandler;
use logger::{Logger, InfoLevel};
use lang;

use std::fs::File;
use std::io::{Write, Read};
use std::path::{Path, PathBuf};
use std::borrow::Cow;
use std::iter::IntoIterator;
use std::thread;

use crossbeam;
use mustache;
use mustache::MapBuilder;
use yaml_rust::{YamlLoader, Yaml};


/// A Book.
///
/// Probably the central structure for of Crowbook, as it is the one
/// that calls the other ones.
///
/// It has the tasks of loading a configuration file, loading chapters
/// and using `Parser`to parse them, and then calling various renderers
/// (`HtmlRendrer`, `LatexRenderer`, `EpubRenderer` and/or `OdtRenderer`)
/// to convert the AST into documents.
///
/// # Examples
///
/// ```
/// use crowbook::{Book, Number};
/// // Create a book with some options
/// let mut book = Book::new(&[("author", "Joan Doe"), ("title", "An untitled book"), ("lang", "en")]);
//
/// // Add content to the book
/// book.add_chapter_as_str(Number::Default, "# The beginning#\nBla, bla, bla").unwrap();
///
/// // Render the book as html to stdout
/// book.render_html(&mut std::io::stdout()).unwrap();
/// ```
pub struct Book {
    /// Internal structure. You should not accesss this directly except if
    /// you are writing a new renderer.
    #[doc(hidden)]
    pub chapters: Vec<(Number, Vec<Token>)>,
    
    /// A list of the filenames of the chapters
    #[doc(hidden)]
    pub filenames: Vec<String>,

    /// Options of the book
    #[doc(hidden)]
    pub options: BookOptions,

    /// Root path of the book
    #[doc(hidden)]
    pub root: PathBuf,

    /// Logger
    #[doc(hidden)]
    pub logger: Logger,

    /// Source for error files
    #[doc(hidden)]
    pub source: Source,
    
    cleaner: Box<Cleaner>,

}

impl Book {
    /// Creates a new `Book` with given options
    ///
    /// # Arguments
    /// *`options` a list (or other iterator) of (key, value) tuples. Can be &[].
    pub fn new<'a,I>(options: I) -> Book
    where I: IntoIterator<Item=&'a(&'a str, &'a str)> {
        let mut book = Book {
            source: Source::empty(),
            chapters: vec!(),
            filenames: vec!(),
            cleaner: Box::new(Off),
            root: PathBuf::new(),
            options: BookOptions::new(),
            logger: Logger::new(InfoLevel::Info),
        };

        // set options
        for &(key, value) in options {
            if let Err(err) = book.options.set(key, value) {
                book.logger.error(format!("Error initializing book: could not set {} to {}: {}", key, value, err));
            }
        }
        // set cleaner according to lang and autoclean settings
        book.update_cleaner();

        book
    }

    /// Creates a new book from a file, with options
    ///
    /// # Arguments
    /// * `filename`: the path of file to load. The directory of this file is used as
    ///   a "root" directory for all paths referenced in books, whether chapter files,
    ///   templates, cover images, and so on.
    /// * `verbosity: sets the book verbosity
    /// * `options`: a list of (key, value) options to pass to the book
    pub fn new_from_file<'a, I> (filename: &str, verbosity: InfoLevel, options: I) -> Result<Book>
    where I:IntoIterator<Item=&'a(&'a str, &'a str)> {
        let mut book = Book::new(options);
        book.source = Source::new(filename);
        book.options.source = Source::new(filename);
        book.logger.set_verbosity(verbosity);
        
        let path = Path::new(filename);
        let mut f = try!(File::open(&path).map_err(|_| Error::file_not_found(Source::empty(),
                                                                             "book",
                                                                             filename.to_owned())));
        // Set book path to book's directory
        if let Some(parent) = path.parent() {
            book.root = parent.to_owned();
            book.options.root = book.root.clone();
        }
        
        let mut s = String::new();
        try!(f.read_to_string(&mut s).map_err(|_| Error::config_parser(Source::new(filename),
                                                                       "file contains invalid UTF-8, could not parse it")));
                                            
        
        let result = book.set_from_config(&s);
        match result {
            Ok(..) => Ok(book),
            Err(err) => {
                if err.is_config_parser() && filename.ends_with(".md") {
                    let err = Error::default(Source::empty(),
                                             format!("could not parse {} as a book file.\nMaybe you meant to run crowbook with the --single argument?",
                                                     &filename));
                    Err(err)
                } else {
                    Err(err)
                }
            }
        }
    }

    /// Creates a book from a single markdown file
    pub fn new_from_markdown_file<'a, I>(filename: &str, verbosity: InfoLevel, options: I) -> Result<Book>
    where I:IntoIterator<Item=&'a(&'a str, &'a str)> {
        let mut book = Book::new(options);
        book.source = Source::new(filename);
        book.logger.set_verbosity(verbosity);

        // Set book path to book's directory
        if let Some(parent) = Path::new(filename).parent() {
            book.root = parent.to_owned();
            book.options.root = book.root.clone();
        }
        book.options.set("tex.short", "true").unwrap();
        book.options.set("input.yaml_blocks", "true").unwrap();

        // Add the file as chapter with hidden title
        // hideous line, but basically transforms foo/bar/baz.md to baz.md
        let relative_path = Path::new(Path::new(filename).components().last().unwrap().as_os_str());
        try!(book.add_chapter(Number::Hidden, &relative_path.to_string_lossy()));

        Ok(book)
    }

    /// Sets options from a YAML block
    fn set_options_from_yaml(&mut self, yaml: &str) -> Result<()> {
        self.options.source = self.source.clone();
        match YamlLoader::load_from_str(&yaml) {
            Err(err) => return Err(Error::config_parser(&self.source,
                                                        format!("YAML block was not valid Yaml: {}", err))),
            Ok(mut docs) => {
                if docs.len() == 1 && docs[0].as_hash().is_some() {
                    if let Yaml::Hash(hash) = docs.pop().unwrap() {
                        for (key,value) in hash.into_iter() {
                            try!(self.options.set_yaml(key, value)); 
                        }
                        Ok(())
                    } else {
                        unreachable!();
                    }
                } else {
                    return Err(Error::config_parser(&self.source,
                                                   "YAML part of the book is not a valid hashmap".to_owned()));
                }
            }
        }
    }
    
    /// Sets options and load chapters according to configuration file
    ///
    /// A line with "option: value" sets the option to value
    ///
    /// + chapter_name.md adds the (default numbered) chapter
    ///
    /// - chapter_name.md adds the (unnumbered) chapter
    ///
    /// 3. chapter_name.md adds the (custom numbered) chapter
    pub fn set_from_config(&mut self, s: &str) -> Result<()> {
        fn get_filename<'a>(source: &Source, s: &'a str) -> Result<&'a str> {
            let words:Vec<&str> = (&s[1..]).split_whitespace().collect();
            if words.len() > 1 {
                return Err(Error::config_parser(source,
                                                "chapter filenames must not contain whitespace".to_owned()));
            } else if words.len() < 1 {
                return Err(Error::config_parser(source,
                                               "no chapter name specified".to_owned()));
            }
            Ok(words[0])
        }

        // Parse the YAML block, that is, until first chapter
        let mut yaml = String::new();
        let mut lines = s.lines().peekable();
        let mut line;

        let mut line_number = 0;
        let mut is_next_line_ok: bool;
        
        loop {
            if let Some(next_line) = lines.peek() {
                if next_line.starts_with(|c| match c {
                    '-'|'+'|'!' => true,
                    _ => c.is_digit(10)
                }) {
                    break;
                } 
            } else {
                break;
            }
            line = lines.next().unwrap();
            line_number += 1;
            self.source.set_line(line_number);
            yaml.push_str(line);
            yaml.push_str("\n");

            if line.trim().ends_with(|c| match c {
                '>' | '|' | ':' | '-' => true,
                _ => false
            }) {
                // line ends with the start of a block indicator
                continue;
            }
            
            if let Some(next_line) = lines.peek() {
                let doc = YamlLoader::load_from_str(next_line);
                if !doc.is_ok() {
                    is_next_line_ok = false;
                } else {
                    let doc = doc.unwrap();
                    if doc.len() > 0 && doc[0].as_hash().is_some() {
                        is_next_line_ok = true;
                    } else {
                        is_next_line_ok = false;
                    }
                }
            } else {
                break;
            }
            if !is_next_line_ok {
                // If next line is not valid yaml, probably means we are in a multistring
                continue;
            }
            let result = self.set_options_from_yaml(&yaml);
            match result {
                Ok(_) => {
                    // Fine, we can remove previous lines
                    yaml = String::new();
                },
                Err(err) => {
                    if err.is_book_option() {
                        // book option error: abort
                        return Err(err);
                    } else {
                        // Other error: we do nothing, hoping it will work
                        // itself out when more lines are added to yaml
                    }
                },
            }
        }
        try!(self.set_options_from_yaml(&yaml));

        // Update cleaner according to options (autoclean/lang)
        self.update_cleaner();
        
        // Parse chapters
        while let Some(line) = lines.next() {
            line_number += 1;
            self.source.set_line(line_number);
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('-') {
                //unnumbered chapter
                let file = try!(get_filename(&self.source, line));
                try!(self.add_chapter(Number::Unnumbered, file));
            } else if line.starts_with('+') {
                //nunmbered chapter
                let file = try!(get_filename(&self.source, line));
                try!(self.add_chapter(Number::Default, file));
            } else if line.starts_with('!') {
                // hidden chapter
                let file = try!(get_filename(&self.source, line));
                try!(self.add_chapter(Number::Hidden, file));
            } else if line.starts_with(|c: char| c.is_digit(10)) {
                // chapter with specific number
                let parts:Vec<_> = line.splitn(2, |c: char| c == '.' || c == ':' || c == '+').collect();
                if parts.len() != 2 {
                    return Err(Error::config_parser(&self.source,
                                                   "ill-formatted line specifying chapter number"));
                } 
                let file = try!(get_filename(&self.source, parts[1]));
                let number = try!(parts[0].parse::<i32>().map_err(|_| Error::config_parser(&self.source,
                                                                                           "Error parsing chapter number")));
                try!(self.add_chapter(Number::Specified(number), file));
            } else {
                return Err(Error::config_parser(&self.source,
                                                "found invalid chapter definition in the chapter list"));
            }
        }

        self.source.unset_line();

        Ok(())
    }

    
    /// Generates output files acccording to book options
    pub fn render_all(&self) -> () {
        let mut handles = vec!();
        crossbeam::scope(|scope| {
            if self.options.get("output.epub").is_ok() {
                handles.push(scope.spawn(|| {
                    let result = self.render_epub();
                    if let Err(err) = result {
                        self.logger.error(format!("rendering EPUB:\n{}", err));
                    }
                }));
            }
            
            if self.options.get("output.html_dir").is_ok() {
                handles.push(scope.spawn(move || {
                    let result = self.render_html_dir();
                    if let Err(err) = result {
                        self.logger.error(format!("rendering HTML directory:\n{}", err));
                    }
                }));
            }
            
            if let Ok(file) = self.options.get_path("output.html") {
                handles.push(scope.spawn(move || {
                    if let Ok(mut f) = File::create(&file) {
                        let result = self.render_html(&mut f);
                        if let Err(err) = result {
                            self.logger.error(format!("rendering HTML:\n{}", err));
                        }
                    } else {
                        self.logger.error(format!("could not create HTML file '{}'", &file));
                    }
                }));
            }
            if let Ok(file) = self.options.get_path("output.tex") {
                handles.push(scope.spawn(move || {
                    if let Ok(mut f) = File::create(&file) {
                        let result = self.render_tex(&mut f);
                        if let Err(err) = result {
                            self.logger.error(format!("rendering LaTeX:\n{}", err));
                        }
                    }
                    else {
                        self.logger.error(format!("could not create LaTeX file '{}'", &file));
                    }
                }));
            }
            if self.options.get("output.pdf").is_ok() {
                handles.push(scope.spawn(|| {
                    let result = self.render_pdf();
                    if let Err(err) = result {
                        self.logger.error(format!("rendering PDF:\n{}", err));
                    }
                }));
            }
            if self.options.get("output.odt").is_ok() {
                handles.push(scope.spawn(|| {
                    let result = self.render_odt();
                    if let Err(err) = result {
                        self.logger.error(format!("rendering PDF:\n{}", err));
                    }
                }));
            }
            if handles.is_empty() {
                self.logger.info("Crowbook generated no file because no output file speficied. Add output.{{format}} to your config file.");
            }
        });
    }


    
    /// Render book to pdf according to book options
    pub fn render_pdf(&self) -> Result<()> {
        self.logger.debug("Attempting to generate pdf...");
        let mut latex = LatexRenderer::new(&self);
        let result = try!(latex.render_pdf());
        self.logger.debug("Output of latex command:");
        self.logger.debug(result);
        self.logger.info(format!("Successfully generated pdf file: {}", self.options.get_path("output.pdf").unwrap()));
        Ok(())
    }

    /// Render book to epub according to book options
    pub fn render_epub(&self) -> Result<()> {
        self.logger.debug("Attempting to generate epub...");
        let mut epub = EpubRenderer::new(&self);
        let result = try!(epub.render_book());
        self.logger.debug("Output of zip command:");
        self.logger.debug(&result);
        self.logger.info(&format!("Successfully generated epub file: {}", self.options.get_path("output.epub").unwrap()));
        Ok(())
    }

    /// Render book to HTML directory according to book options
    pub fn render_html_dir(&self) -> Result<()> {
        self.logger.debug("Attempting to generate html directory...");
        let mut html = HtmlDirRenderer::new(&self);
        try!(html.render_book());
        self.logger.info(&format!("Successfully generated html directory: {}", self.options.get_path("output.html_dir").unwrap()));
        Ok(())
    }

    /// Render book to odt according to book options
    pub fn render_odt(&self) -> Result<()> {
        self.logger.debug("Attempting to generate Odt...");
        let mut odt = OdtRenderer::new(&self);
        let result = try!(odt.render_book());
        self.logger.debug("Output of zip command:");
        self.logger.debug(&result);
        self.logger.info(format!("Successfully generated odt file: {}", self.options.get_path("output.odt").unwrap()));
        Ok(())
    }

    /// Render book to html according to book options
    pub fn render_html<T: Write>(&self, f: &mut T) -> Result<()> {
        self.logger.debug("Attempting to generate HTML...");
        let mut html = HtmlSingleRenderer::new(&self);
        let result = try!(html.render_book());
        try!(f.write_all(&result.as_bytes()).map_err(|e| Error::render(&self.source,
                                                                       format!("problem when writing to HTML file: {}", e))));
        if let Ok(file) = self.options.get_path("output.html") {
            self.logger.info(format!("Successfully generated HTML file: {}", file));
        } else {
            self.logger.info("Successfully generated HTML");
        }
        Ok(())
    }

    /// Render book to pdf according to book options
    pub fn render_tex<T:Write>(&self, f: &mut T) -> Result<()> {
        self.logger.debug("Attempting to generate LaTeX...");

        let mut latex = LatexRenderer::new(&self);
        let result = try!(latex.render_book());
        try!(f.write_all(&result.as_bytes()).map_err(|e| Error::render(&self.source,
                                                                       format!("problem when writing to LaTeX file: {}", e))));
        if let Ok(file) = self.options.get_path("output.tex") {
            self.logger.info(format!("Successfully generated LaTeX file: {}", file));
        } else {
            self.logger.info("Successfully generated LaTeX");
        }
        Ok(())
    }
        
    /// Adds a chapter, as a file name, to the book
    ///
    /// `Book` will then parse the file and store the AST (i.e., a vector
    /// of `Token`s).
    ///
    /// # Arguments
    /// * `number`: specifies if the chapter must be numbered, not numbered, or if its title
    ///   must be hidden. See `Number`.
    /// * `file`: path of the file for this chapter
    ///
    /// **Returns** an error if `file` does not exist, could not be read, of if there was
    /// some error parsing it.
    pub fn add_chapter(&mut self, number: Number, file: &str) -> Result<()> {
        self.logger.debug(&format!("Parsing chapter: {}...", file));
        
        // add file to the list of file names
        self.filenames.push(file.to_owned());


        // try to open file
        let path = self.root.join(file);
        let mut f = try!(File::open(&path).map_err(|_| Error::file_not_found(&self.source,
                                                                             "book chapter",
                                                                             format!("{}", path.display()))));
        let mut s = String::new();
        try!(f.read_to_string(&mut s).map_err(|_| Error::parser(&self.source,
                                                                format!("file {} contains invalid UTF-8", path.display()))));    
            
        // Ignore YAML blocks
        self.parse_yaml(&mut s);
        
        // parse the file
        let mut parser = Parser::new();
        parser.set_source_file(file);
        let mut v = try!(parser.parse(&s));


        // transform the AST to make local links and images relative to `book` directory
        let offset = Path::new(file).parent().unwrap();
        if offset.starts_with("..") {
            self.logger.warning(format!("Warning: book contains chapter '{}' in a directory above the book file, this might cause problems", file));
        }


        // For offset: if nothing is specified, it is the filename's directory
        // If base_path.{images/links} is specified, override it for one of them.
        // If base_path is specified, override it for both.
        let res_base = self.options.get_path("resources.base_path");
        let res_base_img = self.options.get_path("resources.base_path.images");
        let res_base_lnk = self.options.get_path("resources.base_path.links");
        let mut link_offset = offset;
        let mut image_offset = offset;
        if let Ok(ref path) = res_base {
            link_offset = Path::new(path);
            image_offset = Path::new(path);
        } else {
            if let Ok(ref path) = res_base_img {
                image_offset = Path::new(path);
            }
            if let Ok(ref path) = res_base_lnk {
                link_offset = Path::new(path);
            }
        }
        // add offset
        ResourceHandler::add_offset(link_offset.as_ref(), image_offset.as_ref(), &mut v);
        
        self.chapters.push((number, v));
        Ok(())
    }     

    /// Adds a chapter, as a string, to the book
    ///
    /// `Book` will then parse the string and store the AST (i.e., a vector
    /// of `Token`s).
    ///
    /// # Arguments
    /// * `number`: specifies if the chapter must be numbered, not numbered, or if its title
    ///   must be hidden. See `Number`.
    /// * `content`: the content of the chapter.
    ///
    /// **Returns** an error if there was some errror parsing `content`.
    pub fn add_chapter_as_str(&mut self, number: Number, content: &str) -> Result<()> {
        let mut parser = Parser::new();
        let v = try!(parser.parse(content));
        self.chapters.push((number, v));
        self.filenames.push(String::new());
        Ok(())
    }


    /// Either clean a string or does nothing,
    /// according to book `lang` and `autoclean` options
    #[doc(hidden)]
    pub fn clean<'s, S: Into<Cow<'s, str>>>(&self, text: S, tex: bool) -> Cow<'s, str>  {
        self.cleaner.clean(text.into(), tex)
    }

    
    
    /// Returns a template
    ///
    /// Returns the default one if no option was set, or the one set by the user.
    /// 
    /// Returns an error if `template` isn't a valid template name.
    #[doc(hidden)]
    pub fn get_template(&self, template: &str) -> Result<Cow<'static, str>> {
        let option = self.options.get_path(template);
        let fallback = match template {
            "epub.css" => epub::CSS,
            "epub.template" => if try!(self.options.get_i32("epub.version")) == 3 {
                epub3::TEMPLATE
            } else {
                epub::TEMPLATE
            },
            "html.css" => html::CSS,
            "html.print_css" => html::PRINT_CSS,
            "html.template" => html::TEMPLATE,
            "html.script" => html::SCRIPT,
            "html_dir.script" => html_dir::SCRIPT,
            "html_dir.css" => html_dir::CSS,
            "html_dir.index.html" => html_dir::INDEX_HTML,
            "html_dir.chapter.html" => html_dir::CHAPTER_HTML,
            "html.highlight.js" => highlight::JS,
            "html.highlight.css" => highlight::CSS,
            "tex.template" => latex::TEMPLATE,
            _ => return Err(Error::config_parser(&self.source,
                                                format!("invalid template '{}'", template))),
        };
        if let Ok (ref s) = option {
            let mut f = try!(File::open(s).map_err(|_| Error::file_not_found(&self.source,
                                                                             format!("template '{}'", template),
                                                                             s.to_owned())));
            let mut res = String::new();
            try!(f.read_to_string(&mut res)
                 .map_err(|_| Error::config_parser(&self.source,
                                                   format!("file '{}' could not be read", s))));
            Ok(Cow::Owned(res))
        } else {
            Ok(Cow::Borrowed(fallback))
        }
    }


    /// Returns the string corresponding to a number, title, and the numbering template
    #[doc(hidden)]
    pub fn get_header(&self, n: i32, title: &str) -> Result<String> {
        let template = try!(compile_str(self.options.get_str("numbering_template").unwrap(),
                                        &self.source,
                                        "could not compile template 'numbering template'"));
        let data = MapBuilder::new()
            .insert_str("title", String::from(title))
            .insert_str("number", format!("{}", n))
            .build();
        let mut res:Vec<u8> = vec!();
        template.render_data(&mut res, &data);
        match String::from_utf8(res) {
            Err(_) => panic!("header generated by mustache was not valid utf-8"),
            Ok(res) => Ok(res)
        }
    }

    /// Returns a `MapBuilder` (used by `Mustache` for templating), to be used (and completed)
    /// by renderers. It fills it with the metadata options.
    ///
    /// It also uses the lang/xx.yaml file corresponding to the language and fills
    /// `loc_xxx` fiels with it that corresponds to translated versions.
    ///
    /// This method treats the metadata as Markdown and thus calls `f` to render it.
    #[doc(hidden)]
    pub fn get_metadata<F>(&self, mut f: F) -> Result<MapBuilder>
        where F:FnMut(&str)->Result<String> {
        let mut mapbuilder = MapBuilder::new();
        mapbuilder = mapbuilder.insert_str("crowbook_version", env!("CARGO_PKG_VERSION"));

        // Add metadata to mapbuilder
        for key in self.options.get_metadata() {
            if let Ok(s) = self.options.get_str(key) {
                let key = key.replace(".", "_");
                let content = f(s);
                match content {
                    Ok(content) => {
                        mapbuilder = mapbuilder.insert_str(&key, content);
                        mapbuilder = mapbuilder.insert_bool(&format!("has_{}", key), true);
                    },
                    Err(err) => {
                        return Err(Error::render(&self.source,
                                                 format!("could not render `{}` for metadata:\n{}", &key, err)));
                    },
                }
            }
        }

        // Add localization strings
        let hash = lang::get_hash(self.options.get_str("lang").unwrap());
        for (key, value) in hash.into_iter() {
            let key = format!("loc_{}", key.as_str().unwrap());
            let value = value.as_str().unwrap();
            mapbuilder = mapbuilder.insert_str(&key, value);
        }
        Ok(mapbuilder)
    }
        
    /// Remove YAML blocks from a string and try to parse them to set options
    ///
    /// YAML blocks start with
    /// ---
    /// and end either with
    /// ---
    /// or
    /// ... 
    fn parse_yaml(&mut self, content: &mut String) {
        if !(content.starts_with("---\n") || content.contains("\n---\n")
             || content.starts_with("---\r\n") || content.contains("\n---\r\n")) {
            // Content can't contain YAML, so aborting early
            return;
        }
        let mut new_content = String::new();
        let mut previous_empty = true;
        {
            let mut lines = content.lines();
            while let Some(line) = lines.next() {
                if line == "---" && previous_empty {
                    previous_empty = false;
                    let mut yaml_block = String::new();
                    let mut valid_block = false;
                    while let Some(new_line) = lines.next() {
                        if new_line == "---" || new_line == "..." {
                            // Checks that this is valid YAML
                            match YamlLoader::load_from_str(&yaml_block) {
                                Ok(docs) => {
                                    // Use this yaml block to set options only if 1) it is valid
                                    // 2) the option is activated
                                    if docs.len() == 1 && docs[0].as_hash().is_some()
                                        && self.options.get_bool("input.yaml_blocks") == Ok(true) {
                                        let hash = docs[0].as_hash().unwrap();
                                        for (key, value) in hash {
                                            match self.options.set_yaml(key.clone(), value.clone()) { //todo: remove clone
                                                Ok(opt) => {
                                                    if let Some(old_value) = opt {
                                                        self.logger.debug(format!("Inline YAML block replaced {:?} previously set to {:?} to {:?}",
                                                                                  key, old_value, value));
                                                    } else {
                                                        self.logger.debug(format!("Inline YAML block set {:?} to {:?}", key, value));
                                                    }
                                                }
                                                Err(e) => self.logger.error(format!("Inline YAML block could not set {:?} to {:?}: {}", key, value, e)),
                                            }
                                        }
                                    } else {
                                        self.logger.debug(format!("Ignoring YAML block:\n---\n{}---", &yaml_block));
                                    }
                                    valid_block = true;
                                },
                                Err(err) => {
                                    self.logger.error(format!("Found something that looked like a YAML block:\n{}", &yaml_block));
                                    self.logger.error(format!("... but it didn't parse correctly as YAML('{}'), so treating it like Markdown.", err));
                                }
                            }
                            break;
                        } else {
                            yaml_block.push_str(new_line);
                            yaml_block.push_str("\n");
                        }
                    }
                    if !valid_block {
                        // Block was invalid, so add it to markdown content 
                        new_content.push_str(&yaml_block);
                        new_content.push_str("\n");
                    }
                } else if line.is_empty() {
                    previous_empty = true;
                    new_content.push_str("\n");
                } else {
                    previous_empty = false;
                    new_content.push_str(line);
                    new_content.push_str("\n");
                }
            }
        }
        *content = new_content;
        self.update_cleaner();
    }

    // Update the cleaner according to autoclean and lang options
    fn update_cleaner(&mut self) {
        if self.options.get_bool("input.autoclean").unwrap() {
            let lang = self.options.get_str("lang").unwrap().to_lowercase();
            let cleaner: Box<Cleaner> = if lang.starts_with("fr") {
                Box::new(French)
            } else {
                Box::new(Default)
            };
            self.cleaner = cleaner;
        } else {
        self.cleaner = Box::new(Off);
        }
    }
}


/// Calls mustache::compile_str but catches panics and returns a result
pub fn compile_str<O, S>(template: &str, source: O, error_msg: S)  -> Result<mustache::Template>
    where O: Into<Source>,
          S: Into<Cow<'static, str>>
{
    let input: String = template.to_owned();
    let result = thread::spawn(move || mustache::compile_str(&input)).join();
    match result {
        Ok(result) => Ok(result),
        Err(_) => Err(Error::template(source,
                                      error_msg))
    }
}
