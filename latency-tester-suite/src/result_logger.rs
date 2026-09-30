#![allow(dead_code)] // public helpers (verify_file, export_to_csv) are exercised by tests
//! Result logging with cryptographic verification (salted hashes)
//! Ensures results are tamper-proof and verifiable

use anyhow::Result;
use ed25519_dalek::{Signature as EdSignature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Write, Read};
use std::path::Path;


/// Serialize a JSON value with object keys sorted, giving a stable byte representation
fn canonical_json(value: &serde_json::Value) -> Vec<u8> {
    // Keys are sorted at every level, independent of serde_json's `preserve_order` feature
    fn write(v: &serde_json::Value, out: &mut Vec<u8>) {
        match v {
            serde_json::Value::Object(map) => {
                let sorted: std::collections::BTreeMap<_, _> = map.iter().collect();
                out.push(b'{');
                for (i, (k, v)) in sorted.into_iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(serde_json::to_string(k).unwrap_or_default().as_bytes());
                    out.push(b':');
                    write(v, out);
                }
                out.push(b'}');
            }
            serde_json::Value::Array(a) => {
                out.push(b'[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    write(v, out);
                }
                out.push(b']');
            }
            other => out.extend_from_slice(other.to_string().as_bytes()),
        }
    }
    let mut out = Vec::new();
    write(value, &mut out);
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedResult {
    pub header: ResultHeader,
    pub payload: ResultPayload,
    pub signature: Signature,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultHeader {
    pub version: u32,
    pub app_version: String,
    pub app_hash: String,        // SHA256 of the application binary
    pub timestamp: String,       // ISO 8601 UTC
    pub salt: String,            // Hex-encoded salt (32 bytes)
    pub nonce: String,           // Hex-encoded nonce (16 bytes)
    pub algorithm: String,       // "Ed25519"
    pub prev_hash: String,       // SHA256 (hex) of the previous record's signature; chains the log
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultPayload {
    pub test_type: String,       // "memory", "cpu", "gpu", "input", "system"
    /// Kept as raw JSON: the signature covers these exact bytes, so adding fields to `SystemInfo` in a
    /// later version must not change what an older file re-serializes to.
    pub system_info: serde_json::Value,
    pub benchmark_config: serde_json::Value,
    pub benchmark_results: serde_json::Value,
    pub metadata: HashMap<String, String>,
}

/// A few system fields read leniently from the raw JSON (any version of the file)
pub struct SysBrief {
    pub cpu_name: String,
    pub cores: u64,
    pub threads: u64,
    pub memory_gb: f64,
    pub gpu: String,
    pub os: String,
    pub virtualization: bool,
}

impl SysBrief {
    pub fn of(v: &serde_json::Value) -> SysBrief {
        let text = |p: &str| v.pointer(p).and_then(|x| x.as_str()).map(String::from);
        let num = |p: &str| v.pointer(p).and_then(|x| x.as_u64()).unwrap_or(0);
        SysBrief {
            cpu_name: text("/cpu/brand").filter(|b| b != "Unknown").or_else(|| text("/cpu/name")).unwrap_or_else(|| "Unknown".into()),
            cores: num("/cpu/cores"),
            threads: num("/cpu/threads"),
            memory_gb: num("/memory/total") as f64 / 1_073_741_824.0,
            gpu: text("/gpu/name").unwrap_or_else(|| "N/A".into()),
            os: text("/os/name").unwrap_or_else(|| "Unknown".into()),
            virtualization: v.pointer("/virtualization/bios_virtualization_enabled").and_then(|x| x.as_bool()).unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub sig: String,             // Hex-encoded Ed25519 signature
    pub public_key: String,      // Hex-encoded Ed25519 public key of the signer
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub valid: bool,
    pub message: String,
    pub header: Option<ResultHeader>,
    pub payload: Option<ResultPayload>,
}

pub struct ResultLogger {
    pub(crate) app_version: String,
    pub app_hash: String,
    pub(crate) private_key: [u8; 32],  // Ed25519 signing-key seed
    pub(crate) output_dir: String,
}

impl ResultLogger {
    pub fn new(app_version: String, output_dir: String) -> Result<Self> {
        // Compute application binary hash
        let app_hash = Self::compute_app_hash()?;
        
        // Generate or load private key
        let private_key = Self::load_or_generate_key(&output_dir)?;
        
        Ok(Self {
            app_version,
            app_hash,
            private_key,
            output_dir,
        })
    }

    /// Best-effort permission hardening: read-only (and owner-only when `private`)
    fn restrict_file(path: &Path, readable_by_others: bool) {
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_readonly(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                perms.set_mode(if readable_by_others { 0o444 } else { 0o400 });
            }
            #[cfg(not(unix))]
            let _ = readable_by_others; // Windows only has a read-only attribute
            let _ = std::fs::set_permissions(path, perms);
        }
    }

    fn signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.private_key)
    }

    /// Public key (hex) that verifies this logger's results; share it to let others verify
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key().verifying_key().to_bytes())
    }

    /// SHA256 of the last logged record's signature (all zeros for an empty log)
    fn chain_head(&self) -> String {
        match self.load_all_results().ok().and_then(|r| r.last().cloned()) {
            Some(last) => hex::encode(Sha256::digest(last.signature.sig.as_bytes())),
            None => "0".repeat(64),
        }
    }

    pub fn compute_app_hash() -> Result<String> {
        // Get current executable path
        let exe_path = std::env::current_exe()?;
        let mut file = File::open(&exe_path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        let hash = hasher.finalize();
        Ok(hex::encode(hash))
    }

    fn load_or_generate_key(output_dir: &str) -> Result<[u8; 32]> {
        let key_path = Path::new(output_dir).join("signing_key.bin");
        
        if key_path.exists() {
            let mut file = File::open(&key_path)?;
            let mut key = [0u8; 32];
            file.read_exact(&mut key)?;
            Ok(key)
        } else {
            // Generate new key
            let mut key = [0u8; 32];
            OsRng.fill_bytes(&mut key);
            
            // Save key (in production, this should be encrypted with a password)
            std::fs::create_dir_all(output_dir)?;
            let mut file = File::create(&key_path)?;
            file.write_all(&key)?;
            Self::restrict_file(&key_path, false);
            
            Ok(key)
        }
    }

    /// Log a benchmark result with cryptographic signature
    pub fn log_result<T: Serialize>(
        &self,
        test_type: &str,
        system_info: &crate::system_info::SystemInfo,
        config: &T,
        results: &T,
        metadata: HashMap<String, String>,
    ) -> Result<VerifiedResult> {
        // Generate salt and nonce
        let mut salt = [0u8; 32];
        OsRng.fill_bytes(&mut salt);
        
        let mut nonce = [0u8; 16];
        OsRng.fill_bytes(&mut nonce);

        let header = ResultHeader {
            version: 1,
            app_version: self.app_version.clone(),
            app_hash: self.app_hash.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            salt: hex::encode(salt),
            nonce: hex::encode(nonce),
            algorithm: "Ed25519".to_string(),
            prev_hash: self.chain_head(),
        };

        let payload = ResultPayload {
            test_type: test_type.to_string(),
            system_info: serde_json::to_value(system_info)?,
            benchmark_config: serde_json::to_value(config)?,
            benchmark_results: serde_json::to_value(results)?,
            metadata,
        };

        // Create signature
        let signature = self.sign(&header, &payload)?;

        let verified_result = VerifiedResult {
            header,
            payload,
            signature,
        };

        // Write to file
        self.write_result(&verified_result)?;

        Ok(verified_result)
    }

    /// Bytes covered by the signature: salt || nonce || canonical header || canonical payload
    fn signed_bytes(header: &ResultHeader, payload: &ResultPayload) -> Result<Vec<u8>> {
        // Canonical JSON (sorted keys) so the signature survives a save/load round trip,
        // regardless of HashMap iteration order
        let header_bytes = canonical_json(&serde_json::to_value(header)?);
        let payload_bytes = canonical_json(&serde_json::to_value(payload)?);
        let salt = hex::decode(&header.salt)?;
        let nonce = hex::decode(&header.nonce)?;

        let mut data = Vec::new();
        data.extend_from_slice(&salt);
        data.extend_from_slice(&nonce);
        data.extend_from_slice(&header_bytes);
        data.extend_from_slice(&payload_bytes);
        Ok(data)
    }

    fn sign(&self, header: &ResultHeader, payload: &ResultPayload) -> Result<Signature> {
        let data = Self::signed_bytes(header, payload)?;
        let key = self.signing_key();
        let sig = key.sign(&data);

        Ok(Signature {
            sig: hex::encode(sig.to_bytes()),
            public_key: hex::encode(key.verifying_key().to_bytes()),
        })
    }

    fn write_result(&self, result: &VerifiedResult) -> Result<()> {
        std::fs::create_dir_all(&self.output_dir)?;
        
        // Create filename with timestamp and test type
        let timestamp = &result.header.timestamp;
        let test_type = &result.payload.test_type;
        let filename = format!("{}_{}_{}.json", 
            timestamp.replace(':', "-").replace('.', "-"),
            test_type,
            &result.header.nonce[..8]
        );
        
        let filepath = Path::new(&self.output_dir).join(filename);
        
        // Write as JSON (human readable)
        let json = serde_json::to_string_pretty(result)?;
        let mut file = File::create(&filepath)?;
        file.write_all(json.as_bytes())?;
        drop(file);
        // Saved results are immutable: make the file read-only
        Self::restrict_file(&filepath, true);
        
        // Also append to a master log file (length-prefixed JSON records)
        self.append_to_master_log(result)?;
        
        Ok(())
    }

    fn append_to_master_log(&self, result: &VerifiedResult) -> Result<()> {
        let master_log = Path::new(&self.output_dir).join("results_master.log");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(master_log)?;
        
        // Write length-prefixed binary record
        let data = serde_json::to_vec(result)?;
        let len = (data.len() as u32).to_le_bytes();
        file.write_all(&len)?;
        file.write_all(&data)?;
        
        Ok(())
    }

    /// Check the Ed25519 signature of `result` against a trusted public key.
    /// Does not depend on which build produced the result.
    pub fn verify_with_public_key(result: &VerifiedResult, public_key: &[u8; 32]) -> VerificationResult {
        let fail = |message: String| VerificationResult {
            valid: false,
            message,
            header: Some(result.header.clone()),
            payload: Some(result.payload.clone()),
        };
        let trusted = match VerifyingKey::from_bytes(public_key) {
            Ok(k) => k,
            Err(e) => return fail(format!("Invalid public key: {}", e)),
        };
        if result.signature.public_key != hex::encode(public_key) {
            return fail("Result was signed by a different key".to_string());
        }
        let sig_bytes: [u8; 64] = match hex::decode(&result.signature.sig)
            .ok()
            .and_then(|b| b.try_into().ok())
        {
            Some(b) => b,
            None => return fail("Malformed signature".to_string()),
        };
        let data = match Self::signed_bytes(&result.header, &result.payload) {
            Ok(d) => d,
            Err(e) => return fail(format!("Failed to encode result: {}", e)),
        };
        if trusted.verify(&data, &EdSignature::from_bytes(&sig_bytes)).is_err() {
            return fail("Signature verification failed - data may have been tampered with".to_string());
        }
        VerificationResult {
            valid: true,
            message: "Signature valid".to_string(),
            header: Some(result.header.clone()),
            payload: Some(result.payload.clone()),
        }
    }

    /// Verify a result: signature against this logger's key, then the app hash
    pub fn verify_result(&self, result: &VerifiedResult) -> VerificationResult {
        let sig_check = Self::verify_with_public_key(result, &self.signing_key().verifying_key().to_bytes());
        if !sig_check.valid {
            return sig_check;
        }

        // Verify app hash matches current binary
        let current_hash = match Self::compute_app_hash() {
            Ok(h) => h,
            Err(e) => return VerificationResult {
                valid: false,
                message: format!("Failed to compute current app hash: {}", e),
                header: Some(result.header.clone()),
                payload: Some(result.payload.clone()),
            },
        };

        if result.header.app_hash != current_hash {
            return VerificationResult {
                valid: false,
                message: "Application hash mismatch - results may be from a different version".to_string(),
                header: Some(result.header.clone()),
                payload: Some(result.payload.clone()),
            };
        }

        VerificationResult {
            valid: true,
            message: "Verification successful".to_string(),
            header: Some(result.header.clone()),
            payload: Some(result.payload.clone()),
        }
    }

    /// Verify the master log as a whole: every signature, and that the hash chain is
    /// unbroken (detects edited, deleted, inserted or reordered records)
    pub fn verify_log(&self) -> Result<VerificationResult> {
        let results = self.load_all_results()?;
        let mut prev = "0".repeat(64);
        for (i, r) in results.iter().enumerate() {
            let sig = self.verify_result(r);
            if !sig.valid && !sig.message.starts_with("Application hash mismatch") {
                return Ok(VerificationResult { message: format!("Record {}: {}", i + 1, sig.message), ..sig });
            }
            if r.header.prev_hash != prev {
                return Ok(VerificationResult {
                    valid: false,
                    message: format!("Record {}: hash chain broken (record removed, inserted or reordered)", i + 1),
                    header: Some(r.header.clone()),
                    payload: None,
                });
            }
            prev = hex::encode(Sha256::digest(r.signature.sig.as_bytes()));
        }
        Ok(VerificationResult {
            valid: true,
            message: format!("{} record(s) verified, chain intact", results.len()),
            header: None,
            payload: None,
        })
    }

    /// Verify a result from file
    pub fn verify_file(&self, path: &Path) -> Result<VerificationResult> {
        let mut file = File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        
        let result: VerifiedResult = serde_json::from_str(&contents)?;
        Ok(self.verify_result(&result))
    }

    /// Load all results from master log
    pub fn load_all_results(&self) -> Result<Vec<VerifiedResult>> {
        let master_log = Path::new(&self.output_dir).join("results_master.log");
        if !master_log.exists() {
            return Ok(Vec::new());
        }

        let mut file = File::open(master_log)?;
        let mut results = Vec::new();

        loop {
            let mut len_bytes = [0u8; 4];
            match file.read_exact(&mut len_bytes) {
                Ok(_) => {},
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            
            let len = u32::from_le_bytes(len_bytes) as usize;
            let mut data = vec![0u8; len];
            match file.read_exact(&mut data) {
                Ok(_) => {}
                // Truncated trailing record (e.g. interrupted write)
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            
            // Records are length-prefixed, so an unreadable one (e.g. written by an older
            // build in a different format) can be skipped without losing the rest
            match serde_json::from_slice::<VerifiedResult>(&data) {
                Ok(result) => results.push(result),
                Err(e) => tracing::warn!("skipping unreadable record in master log: {}", e),
            }
        }

        Ok(results)
    }

    /// Export results to CSV for analysis
    pub fn export_to_csv(&self, results: &[VerifiedResult], output_path: &Path) -> Result<()> {
        let mut file = File::create(output_path)?;
        let mut writer = csv::Writer::from_writer(&mut file);
        
        // Write header
        writer.write_record(&[
            "timestamp", "test_type", "app_version", "app_hash", "valid",
            "cpu_name", "cpu_cores", "cpu_threads", "memory_total_gb",
            "gpu_name", "os_name", "virtualization_enabled",
        ])?;

        for result in results {
            let verification = self.verify_result(result);
            let sys = SysBrief::of(&result.payload.system_info);
            
            writer.write_record(&[
                &result.header.timestamp,
                &result.payload.test_type,
                &result.header.app_version,
                &result.header.app_hash,
                &verification.valid.to_string(),
                &sys.cpu_name,
                &sys.cores.to_string(),
                &sys.threads.to_string(),
                &format!("{:.2}", sys.memory_gb),
                &sys.gpu,
                &sys.os,
                &sys.virtualization.to_string(),
            ])?;
        }
        
        writer.flush()?;
        Ok(())
    }
}

/// Constant-time string comparison to prevent timing attacks
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let mut result = 0u8;
    for (x, y) in a_bytes.iter().zip(b_bytes.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// Generate a shareable result summary with verification info
pub fn generate_shareable_summary(result: &VerifiedResult, verification: &VerificationResult) -> String {
    let brief = SysBrief::of(&result.payload.system_info);
    format!(
        r#"=== Latency Tester Suite - Verified Result ===
Version: {}
App Version: {}
App Hash: {}
Timestamp: {}
Test Type: {}
Verification: {}
Message: {}

System:
  CPU: {} ({} cores, {} threads)
  Memory: {:.2} GB
  GPU: {}
  OS: {}
  Virtualization: {}

Benchmark Config: {}
Benchmark Results: {}

Signature: {}
Salt: {}
Nonce: {}
"#,
        result.header.version,
        result.header.app_version,
        result.header.app_hash,
        result.header.timestamp,
        result.payload.test_type,
        verification.valid,
        verification.message,
        brief.cpu_name,
        brief.cores,
        brief.threads,
        brief.memory_gb,
        brief.gpu,
        brief.os,
        brief.virtualization,
        serde_json::to_string_pretty(&result.payload.benchmark_config).unwrap_or_default(),
        serde_json::to_string_pretty(&result.payload.benchmark_results).unwrap_or_default(),
        result.signature.sig,
        result.header.salt,
        result.header.nonce,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_logger_creation() {
        let dir = tempdir().unwrap();
        let logger = ResultLogger::new("1.0.0".to_string(), dir.path().to_string_lossy().to_string());
        assert!(logger.is_ok());
    }

    #[test]
    fn test_sign_verify() {
        let dir = tempdir().unwrap();
        let logger = ResultLogger::new("1.0.0".to_string(), dir.path().to_string_lossy().to_string()).unwrap();
        
        let system_info = crate::system_info::SystemInfo {
            cpu: crate::system_info::CpuInfo {
                name: "Test CPU".to_string(),
                vendor: "Test".to_string(),
                brand: "Test".to_string(),
                frequency: 3000,
                max_frequency: 4000,
                cores: 8,
                threads: 16,
                p_cores: 8,
                e_cores: 0,
                l1_cache: 64,
                l2_cache: 1024,
                l3_cache: 16384,
                architecture: "x86_64".to_string(),
                microarchitecture: "Test".to_string(),
                features: vec![],
                current_frequencies: vec![],
                temperatures: vec![],
                power_watts: None,
            },
            memory: crate::system_info::MemoryInfo {
                total: 16 * 1_073_741_824,
                available: 8 * 1_073_741_824,
                used: 8 * 1_073_741_824,
                speed: 3200,
                type_: "DDR4".to_string(),
                channels: 2,
                timings: crate::system_info::MemoryTimings { cl: 16, trcd: 18, trp: 18, tras: 36, trc: 54, voltage: 1.2 },
                modules: vec![],
            },
            gpu: None,
            gpus: vec![],
            motherboard: crate::system_info::MotherboardInfo {
                manufacturer: "Test".to_string(),
                model: "Test".to_string(),
                version: "1.0".to_string(),
                bios_version: "1.0".to_string(),
                bios_date: "2024-01-01".to_string(),
                chipset: "Test".to_string(),
            },
            os: crate::system_info::OsInfo {
                name: "Test".to_string(),
                version: "1.0".to_string(),
                build: "1".to_string(),
                kernel_version: "1.0".to_string(),
                is_virtualized: false,
            },
            virtualization: crate::system_info::VirtualizationInfo {
                bios_virtualization_enabled: true,
                hyper_v_enabled: false,
                vbs_enabled: false,
                hvci_enabled: false,
                wsl_enabled: false,
                kvm_enabled: false,
                vmware_detected: false,
                virtualbox_detected: false,
                kvm_guest: false,
                details: "".to_string(),
            },
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let config = serde_json::json!({"test": "config"});
        let results = serde_json::json!({"test": "results"});
        let metadata = HashMap::new();

        let verified = logger.log_result("test", &system_info, &config, &results, metadata).unwrap();
        let verification = logger.verify_result(&verified);
        
        assert!(verification.valid);
    }

    #[test]
    fn test_tamper_detection() {
        let dir = tempdir().unwrap();
        let logger = ResultLogger::new("1.0.0".to_string(), dir.path().to_string_lossy().to_string()).unwrap();
        
        let system_info = crate::system_info::SystemInfo {
            cpu: crate::system_info::CpuInfo {
                name: "Test CPU".to_string(),
                vendor: "Test".to_string(),
                brand: "Test".to_string(),
                frequency: 3000,
                max_frequency: 4000,
                cores: 8,
                threads: 16,
                p_cores: 8,
                e_cores: 0,
                l1_cache: 64,
                l2_cache: 1024,
                l3_cache: 16384,
                architecture: "x86_64".to_string(),
                microarchitecture: "Test".to_string(),
                features: vec![],
                current_frequencies: vec![],
                temperatures: vec![],
                power_watts: None,
            },
            memory: crate::system_info::MemoryInfo {
                total: 16 * 1_073_741_824,
                available: 8 * 1_073_741_824,
                used: 8 * 1_073_741_824,
                speed: 3200,
                type_: "DDR4".to_string(),
                channels: 2,
                timings: crate::system_info::MemoryTimings { cl: 16, trcd: 18, trp: 18, tras: 36, trc: 54, voltage: 1.2 },
                modules: vec![],
            },
            gpu: None,
            gpus: vec![],
            motherboard: crate::system_info::MotherboardInfo {
                manufacturer: "Test".to_string(),
                model: "Test".to_string(),
                version: "1.0".to_string(),
                bios_version: "1.0".to_string(),
                bios_date: "2024-01-01".to_string(),
                chipset: "Test".to_string(),
            },
            os: crate::system_info::OsInfo {
                name: "Test".to_string(),
                version: "1.0".to_string(),
                build: "1".to_string(),
                kernel_version: "1.0".to_string(),
                is_virtualized: false,
            },
            virtualization: crate::system_info::VirtualizationInfo {
                bios_virtualization_enabled: true,
                hyper_v_enabled: false,
                vbs_enabled: false,
                hvci_enabled: false,
                wsl_enabled: false,
                kvm_enabled: false,
                vmware_detected: false,
                virtualbox_detected: false,
                kvm_guest: false,
                details: "".to_string(),
            },
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let config = serde_json::json!({"test": "config"});
        let results = serde_json::json!({"test": "results"});
        let metadata = HashMap::new();

        let mut verified = logger.log_result("test", &system_info, &config, &results, metadata).unwrap();
        
        // Tamper with results
        verified.payload.benchmark_results = serde_json::json!({"tampered": true});
        
        let verification = logger.verify_result(&verified);
        assert!(!verification.valid);
    }

    fn logger_and_info() -> (tempfile::TempDir, ResultLogger, crate::system_info::SystemInfo) {
        let dir = tempdir().unwrap();
        let logger = ResultLogger::new("1.0.0".to_string(), dir.path().to_string_lossy().to_string()).unwrap();
        let info = crate::system_info::collect_system_info().unwrap();
        (dir, logger, info)
    }

    #[test]
    fn test_file_round_trip_keeps_signature_valid() {
        let (dir, logger, info) = logger_and_info();
        // Floats and several metadata keys: both used to break the signature after reloading
        let results = serde_json::json!({"avg": 0.1 + 0.2, "p99": 1.0e-7, "n": [1.5, 2.25, 1234567.891011]});
        let mut metadata = HashMap::new();
        for k in ["a", "b", "c", "d", "e", "f"] {
            metadata.insert(k.to_string(), k.repeat(3));
        }
        let signed = logger.log_result("cpu", &info, &results, &results, metadata).unwrap();

        let file = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().map_or(false, |e| e == "json"))
            .expect("result file written");
        let verdict = logger.verify_file(&file).unwrap();
        assert!(verdict.valid, "{}", verdict.message);

        let loaded = logger.load_all_results().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].signature.sig, signed.signature.sig);
        assert!(logger.verify_result(&loaded[0]).valid);
    }

    #[test]
    fn test_wrong_key_fails_verification() {
        let (_d, logger, info) = logger_and_info();
        let signed = logger
            .log_result("cpu", &info, &serde_json::json!({}), &serde_json::json!({}), HashMap::new())
            .unwrap();
        let other_dir = tempdir().unwrap();
        let other = ResultLogger::new("1.0.0".to_string(), other_dir.path().to_string_lossy().to_string()).unwrap();
        assert!(!other.verify_result(&signed).valid);
    }

    #[test]
    fn test_key_is_persisted() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_string_lossy().to_string();
        let a = ResultLogger::new("1".into(), path.clone()).unwrap();
        let b = ResultLogger::new("1".into(), path).unwrap();
        assert_eq!(a.private_key, b.private_key);
    }

    #[test]
    fn test_csv_export() {
        let (dir, logger, info) = logger_and_info();
        for t in ["memory", "cpu"] {
            logger.log_result(t, &info, &serde_json::json!({}), &serde_json::json!({}), HashMap::new()).unwrap();
        }
        let results = logger.load_all_results().unwrap();
        let out = dir.path().join("out.csv");
        logger.export_to_csv(&results, &out).unwrap();
        let text = std::fs::read_to_string(out).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.lines().next().unwrap().starts_with("timestamp,test_type"));
    }

    #[test]
    fn test_shareable_summary_mentions_verification() {
        let (_d, logger, info) = logger_and_info();
        let signed = logger
            .log_result("gpu", &info, &serde_json::json!({}), &serde_json::json!({}), HashMap::new())
            .unwrap();
        let v = logger.verify_result(&signed);
        let text = generate_shareable_summary(&signed, &v);
        assert!(text.contains(&signed.signature.sig));
    }

    #[test]
    fn test_load_skips_unreadable_and_truncated_records() {
        let (dir, logger, info) = logger_and_info();
        logger.log_result("cpu", &info, &serde_json::json!({}), &serde_json::json!({}), HashMap::new()).unwrap();
        let log = dir.path().join("results_master.log");
        let mut f = OpenOptions::new().append(true).open(&log).unwrap();
        let junk = b"not json";
        f.write_all(&(junk.len() as u32).to_le_bytes()).unwrap();
        f.write_all(junk).unwrap();
        logger.log_result("gpu", &info, &serde_json::json!({}), &serde_json::json!({}), HashMap::new()).unwrap();
        // truncated final record
        let mut f = OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(&1000u32.to_le_bytes()).unwrap();
        f.write_all(b"short").unwrap();

        let loaded = logger.load_all_results().unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn test_public_key_verification_needs_no_secret() {
        let (dir, logger, info) = logger_and_info();
        let v = serde_json::json!({"x": 1});
        let signed = logger.log_result("cpu", &info, &v, &v, HashMap::new()).unwrap();
        let pk: [u8; 32] = hex::decode(logger.public_key_hex()).unwrap().try_into().unwrap();
        assert!(ResultLogger::verify_with_public_key(&signed, &pk).valid);

        // a different key must not validate it
        let other = tempdir().unwrap();
        let other_logger = ResultLogger::new("1".into(), other.path().to_string_lossy().into()).unwrap();
        let other_pk: [u8; 32] = hex::decode(other_logger.public_key_hex()).unwrap().try_into().unwrap();
        assert!(!ResultLogger::verify_with_public_key(&signed, &other_pk).valid);

        // re-signing edited data with a different key is detected because the key no longer matches
        let mut forged = signed.clone();
        forged.payload.benchmark_results = serde_json::json!({"x": 999});
        forged.signature = other_logger.sign(&forged.header, &forged.payload).unwrap();
        assert!(!ResultLogger::verify_with_public_key(&forged, &pk).valid);
        drop(dir);
    }

    #[test]
    fn test_saved_result_files_are_read_only() {
        let (dir, logger, info) = logger_and_info();
        logger.log_result("cpu", &info, &serde_json::json!({}), &serde_json::json!({}), HashMap::new()).unwrap();
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().map_or(false, |e| e == "json") || path.file_name().unwrap() == "signing_key.bin" {
                assert!(std::fs::metadata(&path).unwrap().permissions().readonly(), "{:?}", path);
                assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err() || is_root());
            }
        }
    }

    fn is_root() -> bool {
        #[cfg(unix)]
        {
            unsafe { libc::geteuid() == 0 }
        }
        #[cfg(not(unix))]
        false
    }

    #[test]
    fn test_hash_chain_detects_tampering() {
        let (dir, logger, info) = logger_and_info();
        let v = serde_json::json!({});
        for t in ["memory", "cpu", "gpu"] {
            logger.log_result(t, &info, &v, &v, HashMap::new()).unwrap();
        }
        assert!(logger.verify_log().unwrap().valid);

        // Delete the middle record from the master log
        let log = dir.path().join("results_master.log");
        let all = logger.load_all_results().unwrap();
        assert_eq!(all[1].header.prev_hash, hex::encode(Sha256::digest(all[0].signature.sig.as_bytes())));
        let mut out = Vec::new();
        for r in [&all[0], &all[2]] {
            let data = serde_json::to_vec(r).unwrap();
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&data);
        }
        std::fs::write(&log, out).unwrap();
        let verdict = logger.verify_log().unwrap();
        assert!(!verdict.valid, "{}", verdict.message);
        assert!(verdict.message.contains("chain"));
    }

    #[test]
    fn test_edited_master_log_record_is_detected() {
        let (dir, logger, info) = logger_and_info();
        let v = serde_json::json!({"x": 1});
        logger.log_result("cpu", &info, &v, &v, HashMap::new()).unwrap();
        let mut r = logger.load_all_results().unwrap().remove(0);
        r.payload.benchmark_results = serde_json::json!({"x": 2});
        let data = serde_json::to_vec(&r).unwrap();
        let mut out = (data.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&data);
        std::fs::write(dir.path().join("results_master.log"), out).unwrap();
        assert!(!logger.verify_log().unwrap().valid);
    }

    #[test]
    fn files_written_before_a_system_info_field_existed_still_verify() {
        let (dir, logger, info) = logger_and_info();
        let v = logger.log_result("cpu", &info, &serde_json::json!({}), &serde_json::json!({"x": 1}), HashMap::new()).unwrap();
        // Simulate an older build: system_info without the newer `gpus` / `kvm_guest` fields, signed as such
        let mut payload = v.payload.clone();
        payload.system_info.as_object_mut().unwrap().remove("gpus");
        payload.system_info["virtualization"].as_object_mut().unwrap().remove("kvm_guest");
        let old = VerifiedResult { header: v.header.clone(), signature: logger.sign(&v.header, &payload).unwrap(), payload };
        let path = dir.path().join("old.json");
        std::fs::write(&path, serde_json::to_string_pretty(&old).unwrap()).unwrap();
        // load it again through the typed structs (as the viewer / verifier does)
        let reloaded: VerifiedResult = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let pk: [u8; 32] = hex::decode(logger.public_key_hex()).unwrap().try_into().unwrap();
        assert!(ResultLogger::verify_with_public_key(&reloaded, &pk).valid, "round trip changed the signed bytes");
        assert!(reloaded.payload.system_info.get("gpus").is_none(), "unknown fields must not be invented on load");
    }
}
