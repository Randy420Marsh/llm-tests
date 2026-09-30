//! Result logging with cryptographic verification (salted hashes)
//! Ensures results are tamper-proof and verifiable

use anyhow::Result;
use hmac::{Hmac, Mac};
use rand::{Rng, RngCore};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Sha256, Sha512, Digest};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write, Read};
use std::path::Path;

type HmacSha256 = Hmac<Sha256>;
type HmacSha512 = Hmac<Sha512>;

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
    pub algorithm: String,       // "HMAC-SHA256" or "HMAC-SHA512"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultPayload {
    pub test_type: String,       // "memory", "cpu", "gpu", "input", "system"
    pub system_info: crate::system_info::SystemInfo,
    pub benchmark_config: serde_json::Value,
    pub benchmark_results: serde_json::Value,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub hmac: String,            // Hex-encoded HMAC
    pub public_key_hash: String, // SHA256 of public key (for future PKI)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub valid: bool,
    pub message: String,
    pub header: Option<ResultHeader>,
    pub payload: Option<ResultPayload>,
}

pub struct ResultLogger {
    app_version: String,
    pub app_hash: String,
    private_key: [u8; 32],  // HMAC key (in production, use proper key management)
    output_dir: String,
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
        let key_path = Path::new(output_dir).join("hmac_key.bin");
        
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
            algorithm: "HMAC-SHA256".to_string(),
        };

        let payload = ResultPayload {
            test_type: test_type.to_string(),
            system_info: system_info.clone(),
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

    fn sign(&self, header: &ResultHeader, payload: &ResultPayload) -> Result<Signature> {
        // Serialize header and payload for signing
        let header_bytes = bincode::serialize(header)?;
        let payload_bytes = bincode::serialize(payload)?;
        
        // Combine with salt and nonce
        let salt = hex::decode(&header.salt)?;
        let nonce = hex::decode(&header.nonce)?;
        
        let mut data = Vec::new();
        data.extend_from_slice(&salt);
        data.extend_from_slice(&nonce);
        data.extend_from_slice(&header_bytes);
        data.extend_from_slice(&payload_bytes);

        // Compute HMAC
        let mut mac = HmacSha256::new_from_slice(&self.private_key)?;
        mac.update(&data);
        let hmac_result = mac.finalize().into_bytes();

        Ok(Signature {
            hmac: hex::encode(hmac_result),
            public_key_hash: hex::encode(Sha256::digest(&self.private_key)),
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
        
        // Also append to a master log file (binary format for efficiency)
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
        let data = bincode::serialize(result)?;
        let len = (data.len() as u32).to_le_bytes();
        file.write_all(&len)?;
        file.write_all(&data)?;
        
        Ok(())
    }

    /// Verify a result file
    pub fn verify_result(&self, result: &VerifiedResult) -> VerificationResult {
        // Recompute signature
        let expected_signature = match self.sign(&result.header, &result.payload) {
            Ok(sig) => sig,
            Err(e) => return VerificationResult {
                valid: false,
                message: format!("Failed to compute signature: {}", e),
                header: None,
                payload: None,
            },
        };

        // Compare HMACs (constant-time comparison)
        if !constant_time_eq(&result.signature.hmac, &expected_signature.hmac) {
            return VerificationResult {
                valid: false,
                message: "HMAC verification failed - data may have been tampered with".to_string(),
                header: Some(result.header.clone()),
                payload: Some(result.payload.clone()),
            };
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
            file.read_exact(&mut data)?;
            
            let result: VerifiedResult = bincode::deserialize(&data)?;
            results.push(result);
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
            let sys = &result.payload.system_info;
            
            writer.write_record(&[
                &result.header.timestamp,
                &result.payload.test_type,
                &result.header.app_version,
                &result.header.app_hash,
                &verification.valid.to_string(),
                &sys.cpu.name,
                &sys.cpu.cores.to_string(),
                &sys.cpu.threads.to_string(),
                &format!("{:.2}", sys.memory.total as f64 / 1_073_741_824.0),
                &sys.gpu.as_ref().map(|g| g.name.clone()).unwrap_or_else(|| "N/A".to_string()),
                &sys.os.name,
                &sys.virtualization.bios_virtualization_enabled.to_string(),
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
        result.payload.system_info.cpu.name,
        result.payload.system_info.cpu.cores,
        result.payload.system_info.cpu.threads,
        result.payload.system_info.memory.total as f64 / 1_073_741_824.0,
        result.payload.system_info.gpu.as_ref().map(|g| g.name.clone()).unwrap_or_else(|| "N/A".to_string()),
        result.payload.system_info.os.name,
        result.payload.system_info.virtualization.bios_virtualization_enabled,
        serde_json::to_string_pretty(&result.payload.benchmark_config).unwrap_or_default(),
        serde_json::to_string_pretty(&result.payload.benchmark_results).unwrap_or_default(),
        result.signature.hmac,
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
}