fn main() {
    nuclei::block_on(async {
        println!("IO runs on {:?}", nuclei::Proactor::backend());
    });
}
