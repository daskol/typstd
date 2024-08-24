use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

use comemo::Prehashed;
use fontdb::Database;
use typst::diag::{FileError, FileResult};
use typst::eval::Tracer;
use typst::foundations::{Bytes, Datetime, Smart};
use typst::model::Document;
use typst::syntax::{FileId, Source, VirtualPath};
use typst::text::{Font, FontBook, FontInfo};
use typst::{Library, World};
use typst_ide::autocomplete;
use typst_ide::CompletionKind;

pub mod package;
pub mod workspace;

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
    /// Path to a root directory. All source files are relative to it.
    root_dir: PathBuf,
    /// Path to main file (usually `main.typ`).
    main_path: PathBuf,
    /// Typst's standard library.
    library: Prehashed<Library>,
    /// Metadata about discovered fonts.
    book: Prehashed<FontBook>,
    /// Locations of and storage for lazily loaded fonts.
    fonts: Vec<LazyFont>,
    /// Source files.
    sources: RefCell<HashMap<PathBuf, Source>>,
    /// Result of compilation.
    document: Arc<Document>,
}

impl LanguageServiceWorld {
    /// Create an evaluation context with main source file `main_path` and all
    /// files located at `root_dir`.
    pub fn new(
        root_dir: &Path,
        main_path: &Path,
        main_text: Option<String>,
    ) -> Option<LanguageServiceWorld> {
        // Read main file or fail.
        let vpath = VirtualPath::within_root(main_path, root_dir)?;
        let file_id = FileId::new(None, vpath);
        let text = main_text.or_else(|| match fs::read(main_path) {
            Ok(bytes) => String::from_utf8(bytes).ok(),
            Err(_) => None,
        })?;
        let source = Source::new(file_id, text);
        let sources = HashMap::<PathBuf, Source>::from([(
            main_path.to_path_buf(),
            source,
        )]);

        let mut db = Database::new();
        db.load_system_fonts();

        let mut book = FontBook::new();
        let mut fonts = Vec::<LazyFont>::new();
        add_embedded_fonts(&mut book, &mut fonts);
        for face in db.faces() {
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
            root_dir: root_dir.to_path_buf(),
            main_path: main_path.to_path_buf(),
            library: Prehashed::new(Library::default()),
            book: Prehashed::new(book),
            fonts: fonts,
            sources: sources.into(),
            document: Default::default(),
        })
    }

    pub fn add_file(&mut self, path: &Path, text: String) {
        // Make FileID (an internal identifier for a file in Typst).
        let root_dir = path.parent().unwrap();
        let vpath = VirtualPath::within_root(path, root_dir).unwrap();
        let id = FileId::new(None, vpath);

        // // Read file content, decode and return it as a source.
        // let body = fs::read(path).unwrap();
        // let text = String::from_utf8(body).unwrap();
        let source = Source::new(id, text);

        self.sources.borrow_mut().insert(path.to_path_buf(), source);
    }

    fn read_source(&self, path: &Path, id: FileId) -> FileResult<Source> {
        // If source is missing then read it from file system.
        log::info!("source(): read source from fs with id={:?}", id);
        match fs::read(path) {
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

    pub fn compile(&mut self) -> Result<(), String> {
        let mut tracer = Tracer::new();
        let result = match typst::compile(self, &mut tracer) {
            Ok(doc) => {
                log::info!("compiled successfully");
                let buffer = typst_pdf::pdf(&doc, Smart::Auto, None);
                let _ = fs::write("main.pdf", buffer).map_err(|err| {
                    log::error!("failed to write PDF file ({err})")
                });
                // Save compiled document in execution context.
                self.document = Arc::new(doc);
                Ok(())
            }
            Err(diag) => {
                let fst = diag.first().unwrap();
                log::warn!("failed to compile: {}", fst.message);
                Err("compilation failed".to_string())
            }
        };
        // Do some garbage collection sweeping out objectes older than N
        // cycles (see typst-cli for details).
        comemo::evict(10);
        result
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
        &self.library
    }

    /// Metadata about all known fonts.
    fn book(&self) -> &Prehashed<FontBook> {
        &self.book
    }

    /// Access the main source file.
    fn main(&self) -> Source {
        log::info!("main(): access to main file: uri={:?}", self.main_path);
        self.sources.borrow().get(&self.main_path).unwrap().clone()
    }

    /// Try to access the specified source file.
    fn source(&self, id: FileId) -> FileResult<Source> {
        log::info!("source(): request source with id={:?}", id);
        let path = match id.package() {
            Some(pkg) => {
                // Get a root directory of the package.
                let version = pkg.version.to_string();
                let pkg_dir = package::prepare_package(&pkg.name, &version)
                    .map_err(|err| {
                        FileError::Other(Some(
                            format!("package failure: {err}").into(),
                        ))
                    })?;

                // Make a path which is relative to a package root.
                pkg_dir.join(id.vpath().as_rootless_path())
            }
            None => self.root_dir.join(id.vpath().as_rootless_path()),
        };

        // Get a real path from FileID (an internal identifier for a file
        // in Typst).
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
            Some(pkg) => {
                // Get a root directory of the package.
                let version = pkg.version.to_string();
                let pkg_dir = package::prepare_package(&pkg.name, &version)
                    .map_err(|err| {
                        FileError::Other(Some(
                            format!("package failure: {err}").into(),
                        ))
                    })?;

                // Read a file which is located at package root.
                let path = pkg_dir.join(id.vpath().as_rootless_path());
                match fs::read(&path) {
                    Ok(bytes) => Ok(Bytes::from(bytes)),
                    Err(_) => Err(FileError::NotFound(path.to_path_buf())),
                }
            }
            None => {
                let path = self.root_dir.join(id.vpath().as_rootless_path());
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
