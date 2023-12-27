use std::fs;

use typst::eval::Tracer;

use typstd::LanguageServiceWorld;

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
