use super::*;

#[test]
fn tu_key_equality() {
    let key1 = TuKey::new("hello", "src/main.c", "debug", "x86_64-linux-musl");
    let key2 = TuKey::new("hello", "src/main.c", "debug", "x86_64-linux-musl");
    let key3 = TuKey::new("hello", "src/main.c", "release", "x86_64-linux-musl");

    assert_eq!(key1, key2);
    assert_ne!(key1, key3);
}

#[test]
fn compile_flags_debug() {
    let flags = CompileFlags::debug();
    let args = flags.to_args();

    assert!(args.contains(&"-O0".to_string()));
    assert!(args.contains(&"-g".to_string()));
    assert!(args.contains(&"-Wall".to_string()));
}

#[test]
fn compile_flags_release() {
    let flags = CompileFlags::release();
    let args = flags.to_args();

    assert!(args.contains(&"-O2".to_string()));
    assert!(!args.contains(&"-g".to_string()));
}

#[test]
fn compile_flags_with_includes() {
    let mut flags = CompileFlags::default();
    flags.include_dirs.push("src".into());
    flags.include_dirs.push("include".into());
    flags.defines.push("DEBUG=1".to_string());

    let args = flags.to_args();

    assert!(args.contains(&"-I".to_string()));
    assert!(args.contains(&"src".to_string()));
    assert!(args.contains(&"-DDEBUG=1".to_string()));
}
