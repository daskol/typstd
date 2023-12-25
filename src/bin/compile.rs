use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use comemo::Prehashed;
use fontdb;
use fontdb::Database;
use typst::diag::{FileError, FileResult};
use typst::eval::Tracer;
use typst::foundations::{Bytes, Datetime};
use typst::syntax::{FileId, Source, VirtualPath};
use typst::text::{Font, FontBook, FontInfo};
use typst::{Library, World};

struct LanguageServiceWorld {
    /// Typst's standard library.
    library: Prehashed<Library>,
    /// Metadata about discovered fonts.
    book: Prehashed<FontBook>,
    /// Locations of and storage for lazily loaded fonts.
    fonts: Vec<Option<Font>>,
}

impl LanguageServiceWorld {
    pub fn new() -> Option<LanguageServiceWorld> {
        let mut db = Database::new();
        db.load_system_fonts();

        let mut fonts = Vec::<Option<Font>>::new();
        let mut book = FontBook::new();
        for (_, face) in db.faces().enumerate() {
            let path = match &face.source {
                fontdb::Source::File(path)
                | fontdb::Source::SharedFile(path, _) => path,
                // We never add binary sources to the database, so there
                // shouln't be any.
                fontdb::Source::Binary(_) => continue,
            };

            let info = db
                .with_face_data(face.id, FontInfo::new)
                .expect("database must contain this font");

            if let Some(info) = info {
                book.push(info);
                //
                let data = fs::read(&path).ok()?.into();
                fonts.push(Font::new(data, face.index));
            }
        }
        Some(Self {
            library: Prehashed::new(Library::build()),
            book: Prehashed::new(book),
            fonts: fonts,
        })
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
        let root_dir = env::current_dir().unwrap_or_default();
        let full_path = root_dir.join(Path::new("main.typ"));
        let path = VirtualPath::within_root(&full_path, &root_dir).unwrap();
        let id = FileId::new(None, path);
        let body = fs::read(full_path).unwrap();
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
        self.fonts[index].clone()
    }

    /// Try to access the font with the given index in the font book.
    fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
        println!("today()");
        Datetime::from_ymd(1970, 1, 1)
    }
}

fn main() {
    let mut world = LanguageServiceWorld::new().unwrap();
    let mut tracer = Tracer::new();
    let result = typst::compile(&mut world, &mut tracer);
    match result {
        Ok(doc) => {
            println!("success!");
            let buffer = typst_pdf::pdf(&doc, None, None);
            let _ = fs::write("main.pdf", buffer)
                .map_err(|err| println!("failed to write PDF file ({err})"));
        }
        Err(diag) => {
            let fst = diag.first().unwrap();
            println!("failed to compiler: {}", fst.message)
        }
    }
}
