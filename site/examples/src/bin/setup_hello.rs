#[nuclei::main]
async fn main() {
    let answer = nuclei::spawn(async { 6 * 7 }).await;
    println!("the answer is {answer}");
}
