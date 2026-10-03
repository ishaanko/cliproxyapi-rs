fn main() {
    let input = std::env::args().nth(1).unwrap_or_default();
    let v = cpa_json::parse_str(&input);
    println!("parsed={} raw_to_string={:?}", v, v.to_string());
    println!("canon={:?}", cpa_core::util::go_json_canonicalize(&input));
    println!("{}", cpa_core::util::clean_json_schema_for_gemini_json_schema(&input));
}
