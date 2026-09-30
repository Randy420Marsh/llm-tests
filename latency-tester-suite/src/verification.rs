#![allow(dead_code)] // public helper API; not every function is wired into the GUI/CLI
//! Verification utilities for result validation and hash checking

use anyhow::Result;
use sha2::{Sha256, Digest};
use std::path::Path;

/// Verify application binary integrity
pub fn verify_app_integrity(expected_hash: &str) -> Result<bool> {
    let exe_path = std::env::current_exe()?;
    let mut file = std::fs::File::open(&exe_path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    let hash = hasher.finalize();
    let actual_hash = hex::encode(hash);
    
    Ok(actual_hash == expected_hash)
}

/// Compute hash of a file
pub fn compute_file_hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

/// Verify a result file against its embedded signature
pub fn verify_result_file(path: &Path, public_key: &[u8]) -> Result<crate::result_logger::VerificationResult> {
    use crate::result_logger::{VerifiedResult, ResultLogger};
    
    let mut file = std::fs::File::open(path)?;
    let mut contents = String::new();
    std::io::Read::read_to_string(&mut file, &mut contents)?;
    
    let result: VerifiedResult = serde_json::from_str(&contents)?;
    
    // Create a temporary logger with the provided public key for verification
    let logger = ResultLogger::new_with_key(
        result.header.app_version.clone(),
        ".".to_string(),
        public_key.try_into().map_err(|_| anyhow::anyhow!("Invalid public key length"))?,
    )?;
    
    Ok(logger.verify_result(&result))
}

/// Generate a hash for the current application binary
pub fn generate_app_hash() -> Result<String> {
    let exe_path = std::env::current_exe()?;
    compute_file_hash(&exe_path)
}

/// Create a signed result package for distribution
pub fn create_signed_package(
    results_dir: &Path,
    output_path: &Path,
    private_key: &[u8; 32],
) -> Result<()> {
    use crate::result_logger::ResultLogger;
    use std::fs::File;
    use std::io::Write;
    
    let logger = ResultLogger::new_with_key(
        env!("CARGO_PKG_VERSION").to_string(),
        results_dir.to_string_lossy().to_string(),
        private_key,
    )?;
    
    let results = logger.load_all_results()?;
    
    // Create a package with all results and a manifest
    let mut package = Vec::new();
    
    // Write manifest
    let manifest = serde_json::json!({
        "version": 1,
        "app_version": env!("CARGO_PKG_VERSION"),
        "app_hash": logger.app_hash.clone(),
        "result_count": results.len(),
        "created": chrono::Utc::now().to_rfc3339(),
    });
    
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    package.extend_from_slice(&(manifest_bytes.len() as u32).to_le_bytes());
    package.extend_from_slice(&manifest_bytes);
    
    // Write each result
    for result in results {
        let result_bytes = serde_json::to_vec(&result)?;
        package.extend_from_slice(&(result_bytes.len() as u32).to_le_bytes());
        package.extend_from_slice(&result_bytes);
    }
    
    // Sign the entire package
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    
    let mut mac = HmacSha256::new_from_slice(private_key)?;
    mac.update(&package);
    let signature = mac.finalize().into_bytes();
    
    // Write signature at the end
    package.extend_from_slice(&signature);
    
    // Write to file
    let mut file = File::create(output_path)?;
    file.write_all(&package)?;
    
    Ok(())
}

/// Verify a signed package
pub fn verify_signed_package(
    package_path: &Path,
    public_key: &[u8; 32],
) -> Result<PackageVerificationResult> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    
    let mut file = std::fs::File::open(package_path)?;
    let mut package = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut package)?;
    
    if package.len() < 32 {
        return Ok(PackageVerificationResult {
            valid: false,
            message: "Package too small".to_string(),
            manifest: None,
            results: Vec::new(),
        });
    }
    
    // Split package and signature
    let (package_data, signature) = package.split_at(package.len() - 32);
    
    // Verify signature
    let mut mac = HmacSha256::new_from_slice(public_key)?;
    mac.update(package_data);
    let expected_signature = mac.finalize().into_bytes();
    
    if !constant_time_eq(signature, &expected_signature) {
        return Ok(PackageVerificationResult {
            valid: false,
            message: "Package signature verification failed".to_string(),
            manifest: None,
            results: Vec::new(),
        });
    }
    
    // Parse package
    let mut offset = 0;
    
    // Read manifest
    let manifest_len = u32::from_le_bytes([
        package_data[offset], package_data[offset+1], 
        package_data[offset+2], package_data[offset+3]
    ]) as usize;
    offset += 4;
    
    let manifest_bytes = &package_data[offset..offset+manifest_len];
    offset += manifest_len;
    
    let manifest: serde_json::Value = serde_json::from_slice(manifest_bytes)?;
    
    // Read results
    let mut results = Vec::new();
    while offset < package_data.len() {
        if offset + 4 > package_data.len() {
            break;
        }
        
        let result_len = u32::from_le_bytes([
            package_data[offset], package_data[offset+1], 
            package_data[offset+2], package_data[offset+3]
        ]) as usize;
        offset += 4;
        
        if offset + result_len > package_data.len() {
            break;
        }
        
        let result_bytes = &package_data[offset..offset+result_len];
        offset += result_len;
        
        let result: crate::result_logger::VerifiedResult = serde_json::from_slice(result_bytes)?;
        results.push(result);
    }
    
    Ok(PackageVerificationResult {
        valid: true,
        message: "Package verified successfully".to_string(),
        manifest: Some(manifest),
        results,
    })
}

#[derive(Debug, Clone)]
pub struct PackageVerificationResult {
    pub valid: bool,
    pub message: String,
    pub manifest: Option<serde_json::Value>,
    pub results: Vec<crate::result_logger::VerifiedResult>,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// ResultLogger extension for verification with custom key
impl crate::result_logger::ResultLogger {
    pub fn new_with_key(
        app_version: String,
        output_dir: String,
        private_key: &[u8; 32],
    ) -> Result<Self> {
        let app_hash = Self::compute_app_hash()?;
        
        Ok(Self {
            app_version,
            app_hash,
            private_key: *private_key,
            output_dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_app_hash() {
        let hash = generate_app_hash();
        assert!(hash.is_ok());
        assert_eq!(hash.unwrap().len(), 64); // SHA256 hex = 64 chars
    }

    fn signed_results(dir: &Path, key: &[u8; 32]) -> ResultLoggerFixture {
        use crate::result_logger::ResultLogger;
        let logger = ResultLogger::new_with_key("1.0.0".into(), dir.to_string_lossy().into(), key).unwrap();
        let info = crate::system_info::collect_system_info().unwrap();
        let v = serde_json::json!({"x": 1.25});
        logger.log_result("cpu", &info, &v, &v, Default::default()).unwrap();
        logger.log_result("gpu", &info, &v, &v, Default::default()).unwrap();
        ResultLoggerFixture
    }
    struct ResultLoggerFixture;

    #[test]
    fn test_signed_package_round_trip_and_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let key = [7u8; 32];
        signed_results(dir.path(), &key);
        let pkg = dir.path().join("package.bin");
        create_signed_package(dir.path(), &pkg, &key).unwrap();

        let ok = verify_signed_package(&pkg, &key).unwrap();
        assert!(ok.valid, "{}", ok.message);
        assert_eq!(ok.results.len(), 2);
        assert_eq!(ok.manifest.unwrap()["result_count"], 2);

        // wrong key
        assert!(!verify_signed_package(&pkg, &[8u8; 32]).unwrap().valid);

        // flipped byte
        let mut bytes = std::fs::read(&pkg).unwrap();
        bytes[10] ^= 0xFF;
        let bad = dir.path().join("bad.bin");
        std::fs::write(&bad, bytes).unwrap();
        assert!(!verify_signed_package(&bad, &key).unwrap().valid);

        // truncated
        let short = dir.path().join("short.bin");
        std::fs::write(&short, [0u8; 8]).unwrap();
        assert!(!verify_signed_package(&short, &key).unwrap().valid);
    }

    #[test]
    fn test_file_hash_matches_known_value() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("abc");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            compute_file_hash(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(verify_app_integrity(&generate_app_hash().unwrap()).unwrap());
        assert!(!verify_app_integrity("deadbeef").unwrap());
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
