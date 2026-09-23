use super::{Error, SecureStorage};

#[test]
fn test_encrypt_decrypt_returns_same_value() {
    let storage = SecureStorage::new("darmok");

    let input = String::from("darmok and jalad at tanagra");
    let encrypted = storage.fallback_encrypt(&input).unwrap();
    let output = storage.fallback_decrypt(&encrypted).unwrap();

    assert_eq!(input, output)
}

#[test]
fn test_encrypt_decrypt_works_across_storage_instances() {
    let storage_1 = SecureStorage::new("darmok");
    let storage_2 = SecureStorage::new("jalad");

    let input = String::from("shaka when the walls fell");
    let encrypted = storage_1.fallback_encrypt(&input).unwrap();
    let output = storage_2.fallback_decrypt(&encrypted).unwrap();

    assert_eq!(input, output)
}

#[test]
fn test_decrypt_fails_on_malformed_data() {
    let storage = SecureStorage::new("darmok");

    let bad_datas: [&[u8]; 4] = [&[], &[0; 1], &[0; 11], &[0; 12]];

    for bad_data in bad_datas {
        let result = storage.fallback_decrypt(bad_data);

        assert!(result.is_err());
        let error = result.unwrap_err();
        let Error::Unknown(err) = error else {
            panic!("Expected error variant to be Error::Unknown, but found {error:?}")
        };
        assert_eq!(
            format!("{err}"),
            "Attempting to decrypt too small value for fallback decryption"
        );
    }
}

#[test]
fn fallback_write_creates_parent_directories_for_nested_keys() {
    // `write_value` falls back to the disk copy when no Secret Service is
    // available; credential keys like `warpi/profile/<id>` contain a path
    // separator, so the writer must create the sub-directory itself.
    use std::os::unix::fs::PermissionsExt as _;

    let temp_dir = tempfile::tempdir().expect("temp dir");
    let fallback_dir = temp_dir.path().join("secure-storage");
    let storage = SecureStorage::new_with_fallback("warpi", fallback_dir);
    let key = "warpi/profile/6e0e0f1a-0000-4000-8000-000000000000";

    storage
        .write_owner_only_fallback_value(key, "sk-test-value")
        .expect("fallback write creates the parent directory");
    assert_eq!(
        storage.read_fallback_value(key).expect("fallback read"),
        "sk-test-value"
    );

    let file = storage.fallback_file(key).expect("fallback file");
    let mode = std::fs::metadata(&file)
        .expect("fallback metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "fallback file must stay owner-only");

    storage.delete_fallback_value(key).expect("fallback delete");
    assert!(!file.exists());
}

#[test]
fn fallback_value_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp_dir = tempfile::tempdir().expect("temp dir");
    let fallback_dir = temp_dir.path().join("secure-storage");
    let storage = SecureStorage::new_with_fallback("darmok", fallback_dir.clone());
    storage
        .write_owner_only_fallback_value("key", "value")
        .expect("fallback write");
    let dir_mode = std::fs::metadata(&fallback_dir)
        .expect("directory metadata")
        .permissions()
        .mode()
        & 0o777;
    let file_mode = std::fs::metadata(storage.fallback_file("key").expect("fallback file"))
        .expect("file metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert_eq!(file_mode, 0o600);
}

#[test]
fn default_fallback_does_not_create_missing_directory() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let fallback_dir = temp_dir.path().join("secure-storage");
    let storage = SecureStorage::new_with_fallback("darmok", fallback_dir.clone());

    assert!(storage.write_fallback_value("key", "value").is_err());
    assert!(!fallback_dir.exists());
}
