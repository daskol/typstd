use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

use comemo::Prehashed;
use fontdb;
use fontdb::Database;
use typst::diag::{FileError, FileResult};
use typst::eval::Tracer;
use typst::foundations::{Bytes, Datetime};
use typst::model::Document;
use typst::syntax::{FileId, Source, VirtualPath};
use typst::text::{Font, FontBook, FontInfo};
use typst::{Library, World};
use typst_ide::autocomplete;
use typst_ide::CompletionKind;

pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionKind,
}

#[derive(Debug)]
pub struct LazyFont {
    path: PathBuf,
    index: u32,
    font: OnceLock<Option<Font>>,
}

impl LazyFont {
    pub fn get(&self) -> Option<Font> {
        self.font
            .get_or_init(|| {
                let data = fs::read(&self.path).ok()?.into();
                Font::new(data, self.index)
            })
            .clone()
    }
}

fn add_embedded_fonts(book: &mut FontBook, fonts: &mut Vec<LazyFont>) {
    let mut process = |bytes: &'static [u8]| {
        let buffer = typst::foundations::Bytes::from_static(bytes);
        for (i, font) in Font::iter(buffer).enumerate() {
            book.push(font.info().clone());
            fonts.push(LazyFont {
                path: PathBuf::new(),
                index: i as u32,
                font: OnceLock::from(Some(font)),
            });
        }
    };

    macro_rules! add {
        ($filename:literal) => {
            process(include_bytes!(concat!("../assets/fonts/", $filename)));
        };
    }

    // Embed default fonts.
    add!("LinLibertine_R.ttf");
    add!("LinLibertine_RB.ttf");
    add!("LinLibertine_RBI.ttf");
    add!("LinLibertine_RI.ttf");
    add!("NewCMMath-Book.otf");
    add!("NewCMMath-Regular.otf");
    add!("NewCM10-Regular.otf");
    add!("NewCM10-Bold.otf");
    add!("NewCM10-Italic.otf");
    add!("NewCM10-BoldItalic.otf");
    add!("DejaVuSansMono.ttf");
    add!("DejaVuSansMono-Bold.ttf");
    add!("DejaVuSansMono-Oblique.ttf");
    add!("DejaVuSansMono-BoldOblique.ttf");
}

/// We should make an assumption that each instance of World corresponds to a
/// specific main fail (=target).
#[derive(Debug)]
pub struct LanguageServiceWorld {
    /// Typst's standard library.
    library: Prehashed<Library>,
    /// Metadata about discovered fonts.
    book: Prehashed<FontBook>,
    /// Locations of and storage for lazily loaded fonts.
    fonts: Vec<LazyFont>,
    /// Source files.
    sources: RefCell<HashMap<PathBuf, Source>>,
    /// Path to main file (usually `main.typ`).
    main_path: Option<PathBuf>,
    /// Result of compilation.
    document: Arc<Document>,
}

impl LanguageServiceWorld {
    pub fn new() -> Option<LanguageServiceWorld> {
        let mut db = Database::new();
        db.load_system_fonts();

        let mut book = FontBook::new();
        let mut fonts = Vec::<LazyFont>::new();
        add_embedded_fonts(&mut book, &mut fonts);
        for (_, face) in db.faces().enumerate() {
            let path = match &face.source {
                fontdb::Source::Binary(_) => continue,
                fontdb::Source::File(path) => path,
                fontdb::Source::SharedFile(path, _) => path,
            };

            let info = db
                .with_face_data(face.id, FontInfo::new)
                .expect("database must contain this font");

            if let Some(info) = info {
                book.push(info);
                fonts.push(LazyFont {
                    path: path.clone(),
                    index: face.index,
                    font: Default::default(),
                });
            }
        }
        Some(Self {
            library: Prehashed::new(Library::build()),
            book: Prehashed::new(book),
            fonts: fonts,
            sources: HashMap::new().into(),
            main_path: None,
            document: Default::default(),
        })
    }

    pub fn add_file(&mut self, path: &Path, text: String) {
        // Make FileID (an internal identifier for a file in Typst).
        let root_dir = path.parent().unwrap();
        let vpath = VirtualPath::within_root(&path, &root_dir).unwrap();
        let id = FileId::new(None, vpath);

        // // Read file content, decode and return it as a source.
        // let body = fs::read(path).unwrap();
        // let text = String::from_utf8(body).unwrap();
        let source = Source::new(id, text);

        self.sources.borrow_mut().insert(path.to_path_buf(), source);
    }

    pub fn add_main_file(&mut self, path: &Path, text: String) {
        self.main_path = Some(path.to_path_buf());
        self.add_file(path, text)
    }

    fn read_source(&self, path: &Path, id: FileId) -> FileResult<Source> {
        // If source is missing then read it from file system.
        log::info!("source(): read source from fs with id={:?}", id);
        match fs::read(&path) {
            Ok(bytes) => String::from_utf8(bytes).map_or(
                Err(FileError::InvalidUtf8),
                |text| {
                    log::info!(
                        "source(): add source with id={:?} to cache",
                        id
                    );
                    let source = Source::new(id, text);
                    self.sources
                        .borrow_mut()
                        .insert(path.to_path_buf(), source.clone());
                    Ok(source)
                },
            ),
            Err(_) => Err(FileError::NotFound(path.to_path_buf())),
        }
    }

    pub fn update_file(
        &mut self,
        path: &Path,
        text: &str,
        begin: (usize, usize),
        end: (usize, usize),
    ) -> Option<Range<usize>> {
        let mut binding = self.sources.borrow_mut();
        let source = binding.get_mut(path)?;
        let begin = source.line_column_to_byte(begin.0, begin.1)?;
        let end = source.line_column_to_byte(end.0, end.1)?;
        let range = Range {
            start: begin,
            end: end,
        };
        Some(source.edit(range, text))
    }

    pub fn compile(&mut self) {
        let mut tracer = Tracer::new();
        let result = typst::compile(self, &mut tracer);
        match result {
            Ok(doc) => {
                log::info!("compiled successfully");
                let buffer = typst_pdf::pdf(&doc, None, None);
                let _ = fs::write("main.pdf", buffer).map_err(|err| {
                    log::error!("failed to write PDF file ({err})")
                });
                // Save compiled document in execution context.
                self.document = Arc::new(doc);
            }
            Err(diag) => {
                let fst = diag.first().unwrap();
                log::warn!("failed to compile: {}", fst.message)
            }
        }
    }

    pub fn complete(
        &mut self,
        path: &Path,
        line: usize,
        column: usize,
    ) -> Vec<CompletionItem> {
        let Some(source) = self.sources.borrow().get(path).cloned() else {
            return vec![];
        };

        let Some(pos) = source.line_column_to_byte(line, column) else {
            return vec![];
        };
        let result = autocomplete(
            self,
            Some(self.document.as_ref()),
            &source,
            pos,
            false,
        );
        match result {
            Some((_, items)) => items
                .iter()
                .map(|el| CompletionItem {
                    label: el.label.to_string(),
                    kind: el.kind.clone(),
                })
                .collect(),
            None => vec![],
        }
    }
}

impl World for LanguageServiceWorld {
    /// The standard library.
    ///
    /// Can be created through `Library::build()`.
    fn library(&self) -> &Prehashed<Library> {
        log::info!("library()");
        &self.library
    }

    /// Metadata about all known fonts.
    fn book(&self) -> &Prehashed<FontBook> {
        log::info!("book()");
        &self.book
    }

    /// Access the main source file.
    fn main(&self) -> Source {
        log::info!("main(): access to main file: uri={:?}", self.main_path);
        let main_path = match &self.main_path {
            Some(path) => path.as_path(),
            None => panic!("no path to main file"),
        };
        self.sources.borrow().get(main_path).unwrap().clone()
    }

    /// Try to access the specified source file.
    fn source(&self, id: FileId) -> FileResult<Source> {
        log::info!("source(): request source with id={:?}", id);
        if id.package().is_some() {
            return Err(FileError::NotFound(PathBuf::new()));
        }

        // Get a real path from FileID (an internal identifier for a file
        // in Typst).
        let main_path = self.main_path.clone().unwrap();
        let root_dir = main_path.parent().unwrap();
        let path = root_dir.join(id.vpath().as_rootless_path());
        log::info!("source(): look up a source with id={:?} at {:?}", id, path);

        // Look up a source by its absolute path.
        {
            let binding = self.sources.borrow();
            let result = binding.get(&path);
            if result.is_some() {
                log::info!("source(): found source with id={:?}", id);
                return Ok(result.unwrap().clone());
            }
        };
        self.read_source(&path, id)
    }

    /// Try to access the specified file.
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        log::info!("file(): request file with id={:?} ", id);
        match id.package() {
            Some(package) => {
                log::info!("(package={}; vpath={:?})", package, id.vpath());
                Err(FileError::NotFound(PathBuf::new()))
            }
            None => {
                let Some(main_path) = self.main_path.clone() else {
                    return Err(FileError::Other(Some(
                        "missing main path".into(),
                    )));
                };
                let Some(root_dir) = main_path.parent() else {
                    return Err(FileError::NotFound(
                        id.vpath().as_rootless_path().to_path_buf(),
                    ));
                };
                let path = root_dir.join(id.vpath().as_rootless_path());
                match fs::read(&path) {
                    Ok(bytes) => Ok(Bytes::from(bytes)),
                    Err(_) => Err(FileError::NotFound(path.to_path_buf())),
                }
            }
        }
    }

    /// Try to access the font with the given index in the font book.
    fn font(&self, index: usize) -> Option<Font> {
        log::debug!("font(): index={}", index);
        self.fonts[index].get()
    }

    /// Try to access the font with the given index in the font book.
    fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
        log::info!("today()");
        Datetime::from_ymd(1970, 1, 1)
    }
}
