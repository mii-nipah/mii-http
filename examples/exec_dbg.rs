fn main() {
    use mii_http::parse::exec::parse_exec;
    let cases = [
        "echo Hello, [%name] [%guest]",
        "$ | xargs echo",
        "echo title=[$.title] count=[$.count]",
        "echo user [:user_id]",
    ];
    for c in &cases {
        println!("--- {}", c);
        match parse_exec(c, 0) {
            Ok(stages) => {
                for s in &stages {
                    println!("  {:#?}", s);
                }
            }
            Err(e) => println!("  ERR: {} @ {:?}", e.message, e.span),
        }
    }
}
