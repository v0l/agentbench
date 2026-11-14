use agent_voice::speech::phonon::text::{Lang, Normalize};
fn main() {
    let text = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let n = Normalize::for_lang(Lang::En);
    for line in text.lines() {
        println!("{}", serde_json::to_string(&n.apply(line).to_string()).unwrap());
    }
}
