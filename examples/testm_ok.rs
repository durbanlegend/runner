{ let mut map: std::collections::HashMap<String,String> = HashMap::new();
    map.insert("hello".to_string(),"world".to_string());
    let option = map.insert("hello".to_string(),"dolly".to_string());
    assert!(option.expect("Failed") == "world");
    println!("Well that worked");
}
