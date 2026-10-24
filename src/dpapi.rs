use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::w;

const ENTROPY: &[u8] = b"PS5 LAN Capture/registration/v1";

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    }
}

fn take_output(output: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe { LocalFree(Some(HLOCAL(output.pbData as _))) };
    bytes
}

pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    let input = blob(plain);
    let entropy = blob(ENTROPY);
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            w!("PS5 LAN Capture"),
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|e| format!("DPAPI: {e}"))?;
    Ok(take_output(output))
}

pub fn unprotect(cipher: &[u8]) -> Result<Vec<u8>, String> {
    let input = blob(cipher);
    let entropy = blob(ENTROPY);
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|e| format!("DPAPI: {e}"))?;
    Ok(take_output(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let secret = b"regist-key-0123456789";
        let cipher = protect(secret).unwrap();
        assert_ne!(cipher.as_slice(), secret.as_slice());
        assert_eq!(unprotect(&cipher).unwrap(), secret);
    }

    #[test]
    fn tampered_cipher_is_rejected() {
        let mut cipher = protect(b"abc").unwrap();
        let last = cipher.len() - 1;
        cipher[last] ^= 0xff;
        assert!(unprotect(&cipher).is_err());
    }
}
