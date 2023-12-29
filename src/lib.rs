use std::collections::HashMap;
use std::fs;
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
    sources: HashMap<PathBuf, String>,
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
            sources: HashMap::new(),
            main_path: None,
            document: Default::default(),
        })
    }

    pub fn add_file(&mut self, path: &Path, text: String) {
        self.sources.insert(path.to_path_buf(), text);
    }

    pub fn add_main_file(&mut self, path: &Path, text: String) {
        self.main_path = Some(path.to_path_buf());
        self.add_file(path, text)
    }

    pub fn compile(&mut self) {
        let mut tracer = Tracer::new();
        let result = typst::compile(self, &mut tracer);
        match result {
            Ok(doc) => {
                log::info!("compiled successfully");
                let buffer = typst_pdf::pdf(&doc, None, None);
                let _ = fs::write("main.pdf", buffer).map_err(|err| {
                    println!("failed to write PDF file ({err})")
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
        line: usize,
        column: usize,
    ) -> Vec<CompletionItem> {
        let source = self.main();
        let pos = match source.line_column_to_byte(line, column) {
            Some(pos) => pos,
            None => return vec![],
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
        println!("library()");
        &self.library
    }

    /// Metadata about all known fonts.
    fn book(&self) -> &Prehashed<FontBook> {
        println!("book()");
        &self.book
    }

    /// Access the main source file.
    fn main(&self) -> Source {
        println!("main()");
        let main_path = match &self.main_path {
            Some(path) => path.as_path(),
            None => panic!("no path to main file"),
        };

        // Make FileID (an internal identifier for a file in Typst).
        let root_dir = main_path.parent().unwrap();
        let vpath = VirtualPath::within_root(&main_path, &root_dir).unwrap();
        let id = FileId::new(None, vpath);

        // Read file content, decode and return it as a source.
        let body = fs::read(main_path).unwrap();
        let text = String::from_utf8(body).unwrap();
        Source::new(id, text)
    }

    /// Try to access the specified source file.
    fn source(&self, id: FileId) -> FileResult<Source> {
        print!("source(): id={:?} ", id);
        if id.package().is_some() {
            return Err(FileError::NotFound(PathBuf::new()));
        }
        let real_path = Path::new(id.vpath().as_rootless_path());
        match fs::read(real_path) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_or(Err(FileError::InvalidUtf8), |text| {
                    Ok(Source::new(id, text))
                }),
            Err(_) => Err(FileError::NotFound(real_path.to_path_buf())),
        }
    }

    /// Try to access the specified file.
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        print!("file(): id={:?} ", id);
        match id.package() {
            Some(package) => {
                println!("(package={}; vpath={:?})", package, id.vpath())
            }
            None => {
                println!("(vpath={:?})", id.vpath())
            }
        }
        Err(FileError::NotFound(PathBuf::new()))
    }

    /// Try to access the font with the given index in the font book.
    fn font(&self, index: usize) -> Option<Font> {
        println!("font(): index={}", index);
        self.fonts[index].get()
    }

    /// Try to access the font with the given index in the font book.
    fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
        println!("today()");
        Datetime::from_ymd(1970, 1, 1)
    }
}
