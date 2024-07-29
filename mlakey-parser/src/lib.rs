use der_parser::der::*;
use der_parser::error::BerError;

use der_parser::oid::Oid;
use der_parser::*;
use nom::combinator::{complete, eof};
use nom::IResult;

use std::convert::{From, TryInto};

use curve25519_dalek::edwards::CompressedEdwardsY;
use curve25519_dalek::montgomery::MontgomeryPoint;
use ml_kem::EncodedSizeUser;
use rand_core::{CryptoRng, RngCore};
use sha2::{Digest, Sha512};
// Re-export x25519_dalek structures for convenience
pub use x25519_dalek::{PublicKey, StaticSecret};

use std::fmt;

use mla::crypto::hybrid::{
    HybridPrivateKey, HybridPublicKey, MLKEMDecapsulationKey, MLKEMEncapsulationKey,
};

const ED_25519_OID: Oid<'static> = oid!(1.3.101 .112);
const X_25519_OID: Oid<'static> = oid!(1.3.101 .110);
const ECC_PRIVKEY_SIZE: usize = 32;
const ECC_PUBKEY_SIZE: usize = 32;

// TODO: update with actual OID once attribued by NIST/IANA
// For now, use a ANSSI factice OID
// https://datatracker.ietf.org/doc/html/draft-ietf-lamps-kyber-certificates
const MLKEM_1024_OID: Oid<'static> = oid!(1.2.250 .1 .223 .201);

const MLKEM_1024_PUBKEY_SIZE: usize = 1568;
const MLKEM_1024_PRIVKEY_SIZE: usize = 3168;

// ---- Error handling ----

#[derive(Debug)]
pub enum MLAKeyParserError {
    /// BER Parsing error (wrong tag, not enough DER elements, etc.)
    BerError(der_parser::error::BerError),
    /// PEM Parsing error
    PemError(pem::PemError),
    /// Nom parsing error (wrong format, unexpected elements, etc.)
    NomError(nom::Err<der_parser::error::BerError>),
    UnknownOid,
    InvalidData,
    InvalidPEMTag,
}
impl From<der_parser::error::BerError> for MLAKeyParserError {
    fn from(error: der_parser::error::BerError) -> Self {
        MLAKeyParserError::BerError(error)
    }
}

impl From<pem::PemError> for MLAKeyParserError {
    fn from(error: pem::PemError) -> Self {
        MLAKeyParserError::PemError(error)
    }
}

impl From<nom::Err<der_parser::error::BerError>> for MLAKeyParserError {
    fn from(error: nom::Err<der_parser::error::BerError>) -> Self {
        MLAKeyParserError::NomError(error)
    }
}

impl fmt::Display for MLAKeyParserError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // For now, use the debug derived version
        write!(f, "{self:?}")
    }
}

// ---- Commons ----

#[derive(Debug, PartialEq)]
struct DerTag<'a> {
    tag: DerObject<'a>,
}

#[derive(Debug, PartialEq)]
struct DerStruct<'a> {
    header: DerTag<'a>,
    data: DerObject<'a>,
}

/// Parse the following structure:
/// ```ascii
/// Seq(
///     OID(tag)    
/// )
/// ```
/// Return the corresponding tag
fn parse_seq_oid(i: &[u8]) -> IResult<&[u8], DerTag, BerError> {
    parse_der_container(|i: &[u8], hdr| {
        if hdr.tag() != Tag::Sequence {
            return Err(nom::Err::Error(BerError::InvalidTag));
        }
        let (i, tag) = parse_der_oid(i)?;
        eof(i)?;
        Ok((i, DerTag { tag }))
    })(i)
}

// ---- Private key ----

/// Parse the following structure:
/// ```ascii
/// Seq(
///     Int, (ignored)
///     Seq(
///         OID(tag)
///     ),
///     OctetString(data)
/// )
/// ```
fn parse_seq_int_tag_octetstring(i: &[u8]) -> IResult<&[u8], DerStruct, BerError> {
    parse_der_container(|i: &[u8], hdr| {
        if hdr.tag() != Tag::Sequence {
            return Err(nom::Err::Error(BerError::InvalidTag));
        }
        let (i, _unk) = parse_der_integer(i)?;
        let (i, header) = complete(parse_seq_oid)(i)?;
        let (i, data) = parse_der_octetstring(i)?;
        eof(i)?;
        Ok((i, DerStruct { header, data }))
    })(i)
}

const TAG_OCTETSTRING: u8 = 4;

/// Take a parsed DER Sequence of a ED25519 or X25519 private key, and return the corresponding
/// `x25519_dalek::StaticSecret`
///
/// Expected structure (OpenSSL X25519):
///
/// ASN1:
///    0:d=0  hl=2 l=  46 cons: SEQUENCE
///    2:d=1  hl=2 l=   1 prim: INTEGER           :00
///    5:d=1  hl=2 l=   5 cons: SEQUENCE
///    7:d=2  hl=2 l=   3 prim: OBJECT            :ED25519 OR :X25519
///   12:d=1  hl=2 l=  34 prim: OCTET STRING
///
/// Tree view:
/// Seq(
///     Int,
///     Seq(
///         OID(1.3.101.112), // ED25519 OR OID(1.3.101.110), // X25519
///     ),
///     OctetString(TAG_OCTETSTRING + LENGTH + DATA),
/// )
///
/// From RFC8032, to obtain the corresponding `x25519_dalek::StaticSecret` from an ed25519 key:
///   `clamping(Sha512(DATA)[0..32])`
/// with `clamping` operation already done on `StaticSecret` creation
fn parse_openssl_25519_privkey_internal(
    private: DerStruct,
) -> Result<StaticSecret, MLAKeyParserError> {
    let data = private.data.content.as_slice()?;
    // data[0] == TAG_OCTETSTRING(4)
    // data[1] == LENGTH
    if data.len() != 2 + ECC_PRIVKEY_SIZE
        || data[0] != TAG_OCTETSTRING
        || data[1] != ECC_PRIVKEY_SIZE as u8
    {
        return Err(MLAKeyParserError::InvalidData);
    }
    let mut key_data = [0u8; ECC_PRIVKEY_SIZE];

    let read_oid = private.header.tag.as_oid()?;
    if read_oid == &ED_25519_OID {
        key_data
            .copy_from_slice(&Sha512::digest(&data[2..2 + ECC_PRIVKEY_SIZE])[0..ECC_PRIVKEY_SIZE]);
    } else if read_oid == &X_25519_OID {
        key_data.copy_from_slice(&data[2..2 + ECC_PRIVKEY_SIZE]);
    } else {
        return Err(MLAKeyParserError::UnknownOid);
    }
    Ok(StaticSecret::from(key_data))
}

/// Take a parsed DER Sequence of a MLA private key, and return the corresponding
/// `mla::crypto::hybrid::MLKEMDecapsulationKey`
///
/// Expected structure:
/// Seq(
///     Int,
///     Seq(
///         OID(1.2.250.1.223.201)
///     ),
///     OctetString(private key)
/// )
fn parse_mlkem_decapkey_internal(
    private: DerStruct,
) -> Result<MLKEMDecapsulationKey, MLAKeyParserError> {
    let data = private.data.content.as_slice()?;
    let read_oid = private.header.tag.as_oid()?;
    if read_oid != &MLKEM_1024_OID {
        return Err(MLAKeyParserError::UnknownOid);
    }
    if data.len() != MLKEM_1024_PRIVKEY_SIZE {
        return Err(MLAKeyParserError::InvalidData);
    }
    Ok(MLKEMDecapsulationKey::from_bytes(
        data.try_into()
            .map_err(|_| MLAKeyParserError::InvalidData)?,
    ))
}

/// Expected structure:
///
/// - ASN1:
/// ```ascii
///     0:d=0  hl=4 l=3239 cons: SEQUENCE          
///     4:d=1  hl=2 l=  46 cons:  SEQUENCE          
///     6:d=2  hl=2 l=   1 prim:   INTEGER           :00
///     9:d=2  hl=2 l=   5 cons:   SEQUENCE          
///     11:d=3  hl=2 l=   3 prim:    OBJECT            :X25519
///     16:d=2  hl=2 l=  34 prim:   OCTET STRING      [HEX DUMP]:...
///     52:d=1  hl=4 l=3187 cons:  SEQUENCE          
///     56:d=2  hl=2 l=   1 prim:   INTEGER           :01
///     59:d=2  hl=2 l=  10 cons:   SEQUENCE          
///     61:d=3  hl=2 l=   8 prim:    OBJECT            :1.2.250.1.223.201
///     71:d=2  hl=4 l=3168 prim:   OCTET STRING      [HEX DUMP]:...
/// ```
///
/// Tree view:
/// Seq(
///     Seq(
///         Int,
///         Seq(
///             OID(1.3.101.112), // ED25519 OR OID(1.3.101.110), // X25519
///         ),
///         OctetString(TAG_OCTETSTRING + LENGTH + DATA),
///     ),
///     Seq(
///         Int,
///         Seq(
///             OID(1.2.250.1.223.201)
///         ),
///         OctetString(private key)
///     )
/// )
///
/// Note: OID order can change (Ed/X 25519 then MLKEM, MLKEM then Ed/X 25519)
pub fn parse_mlakey_privkey_der(data: &[u8]) -> Result<HybridPrivateKey, MLAKeyParserError> {
    let (_remain, (seq_25519, seq_mlkem)) = parse_der_container(|i: &[u8], hdr| {
        if hdr.tag() != Tag::Sequence {
            return Err(nom::Err::Error(BerError::InvalidTag));
        }
        // Expect a Seq(Seq, Seq)
        let (i, seq1) = parse_seq_int_tag_octetstring(i)?;
        let (i, seq2) = parse_seq_int_tag_octetstring(i)?;
        eof(i)?;

        // OID order can change
        // This is not the real OID check, only for variable order
        let (seq_25519, seq_mlkem) = {
            if seq1.header.tag.as_oid()? == &MLKEM_1024_OID {
                // MLKEM, then Ed/X 25519
                (seq2, seq1)
            } else {
                // Ed/X 25519 then MLKEM
                (seq1, seq2)
            }
        };

        Ok((i, (seq_25519, seq_mlkem)))
    })(data)?;

    // Parse ML-KEM part
    let private_key_ml = parse_mlkem_decapkey_internal(seq_mlkem)?;

    // Parse X/Ed 25519 part
    let private_key_ecc = parse_openssl_25519_privkey_internal(seq_25519)?;

    Ok(HybridPrivateKey {
        private_key_ecc,
        private_key_ml,
    })
}

/// Parse a DER ED25519 or X25519 private key, and return the corresponding
/// `x25519_dalek::StaticSecret`
pub fn parse_openssl_25519_privkey_der(data: &[u8]) -> Result<StaticSecret, MLAKeyParserError> {
    let (_remain, private) = parse_seq_int_tag_octetstring(data)?;
    parse_openssl_25519_privkey_internal(private)
}

// ---- Public key ----

/// Parse the following structure:
/// ```ascii
/// Seq(
///    Seq(
///       OID(tag)
///    ),
///    BitString(data)
/// )
/// ```
/// Return the corresponding tag and data
fn parse_seq_tag_bitstring(i: &[u8]) -> IResult<&[u8], DerStruct, BerError> {
    parse_der_container(|i: &[u8], hdr| {
        if hdr.tag() != Tag::Sequence {
            return Err(nom::Err::Error(BerError::InvalidTag));
        }
        let (i, header) = complete(parse_seq_oid)(i)?;
        let (i, data) = parse_der_bitstring(i)?;
        eof(i)?;
        Ok((i, DerStruct { header, data }))
    })(i)
}

/// Parse the following structure:
/// ```ascii
/// Seq(
///    Seq(
///       OID(tag)
///    ),
///    OctetString(data)
/// )
/// ```
/// Return the corresponding tag and data
fn parse_seq_tag_octetstring(i: &[u8]) -> IResult<&[u8], DerStruct, BerError> {
    parse_der_container(|i: &[u8], hdr| {
        if hdr.tag() != Tag::Sequence {
            return Err(nom::Err::Error(BerError::InvalidTag));
        }
        let (i, header) = complete(parse_seq_oid)(i)?;
        let (i, data) = parse_der_octetstring(i)?;
        eof(i)?;
        Ok((i, DerStruct { header, data }))
    })(i)
}

/// Take a parsed DER Sequence of a Ed25519 or X25519 public key, and return the corresponding
/// `x25519_dalek::PublicKey`
///
/// Expected structure:
///
/// ASN1:
///  0:d=0  hl=2 l=  42 cons: SEQUENCE
///  2:d=1  hl=2 l=   5 cons: SEQUENCE
///  4:d=2  hl=2 l=   3 prim: OBJECT            :ED25519
///  9:d=1  hl=2 l=  33 prim: BIT STRING
///
/// Tree view:
/// Seq(
///     Seq(
///         OID(1.3.101.112), // ED25519  OR  OID(1.3.101.110), // X25519
///     ),
///     BitString(DATA),
/// )
///
/// From RFC8032 and OpenSSL format, to obtain the corresponding
/// `x25519_dalek::PublicKey`, which internally use the Montgomery form
/// from an Ed25519 key:
///   to_montgomery(decompress_edwardspoint(DATA))
fn parse_openssl_25519_pubkey_internal(
    ed25519_public: DerStruct,
) -> Result<PublicKey, MLAKeyParserError> {
    let data = ed25519_public.data.content.as_slice()?;
    let data: [u8; ECC_PUBKEY_SIZE] = data
        .try_into()
        .map_err(|_| MLAKeyParserError::InvalidData)?;
    let read_oid = ed25519_public.header.tag.as_oid()?;
    if read_oid == &ED_25519_OID {
        CompressedEdwardsY::from_slice(&data)
            .ok()
            .and_then(|c| c.decompress())
            .map(|v| PublicKey::from(v.to_montgomery().to_bytes()))
            .ok_or(MLAKeyParserError::InvalidData)
    } else if read_oid == &X_25519_OID {
        Ok(PublicKey::from(MontgomeryPoint(data).to_bytes()))
    } else {
        Err(MLAKeyParserError::UnknownOid)
    }
}

/// Parse a DER Ed25519 or X25519 public key, and return the corresponding
/// `x25519_dalek::PublicKey`
pub fn parse_openssl_25519_pubkey_der(data: &[u8]) -> Result<PublicKey, MLAKeyParserError> {
    let (_remain, ed25519_public) = parse_seq_tag_bitstring(data)?;
    parse_openssl_25519_pubkey_internal(ed25519_public)
}

/// Parse a DER MLA public key, and return the corresponding
/// `mla::crypto::hybrid::MLKEMEncapsulationKey`
///
/// Expected structure:
/// Seq(
///    Seq(
///      OID(1.2.250.1.223.201)
///    ),
///    OctetString(data)
/// )
fn parse_mlkem_encapkey_internal(
    public: DerStruct,
) -> Result<MLKEMEncapsulationKey, MLAKeyParserError> {
    let data = public.data.content.as_slice()?;
    let read_oid = public.header.tag.as_oid()?;
    if read_oid != &MLKEM_1024_OID {
        return Err(MLAKeyParserError::UnknownOid);
    }
    if data.len() != MLKEM_1024_PUBKEY_SIZE {
        return Err(MLAKeyParserError::InvalidData);
    }
    Ok(MLKEMEncapsulationKey::from_bytes(
        data.try_into()
            .map_err(|_| MLAKeyParserError::InvalidData)?,
    ))
}

/// Expected structure:
///
/// - ASN1:
/// ```ascii
///     0:d=0  hl=4 l=1632 cons: SEQUENCE          
///     4:d=1  hl=2 l=  42 cons:  SEQUENCE          
///     6:d=2  hl=2 l=   5 cons:   SEQUENCE          
///     8:d=3  hl=2 l=   3 prim:    OBJECT            :X25519
///     13:d=2  hl=2 l=  33 prim:   BIT STRING        
///     48:d=1  hl=4 l=1584 cons:  SEQUENCE          
///     52:d=2  hl=2 l=  10 cons:   SEQUENCE          
///     54:d=3  hl=2 l=   8 prim:    OBJECT            :1.2.250.1.223.201
///     64:d=2  hl=4 l=1568 prim:   OCTET STRING      [HEX DUMP]:...
/// ```
///
/// Tree view:
/// Seq(
///     Seq(
///         Seq(
///             OID(1.3.101.112), // ED25519 OR OID(1.3.101.110), // X25519
///         ),
///         BiString(DATA),
///     ),
///     Seq(
///         Seq(
///             OID(1.2.250.1.223.201)
///         ),
///         OctetString(public key)
///     )
/// )
///
/// Note: OID order can change (Ed/X 25519 then MLKEM, MLKEM then Ed/X 25519)
pub fn parse_mlakey_pubkey_der(data: &[u8]) -> Result<HybridPublicKey, MLAKeyParserError> {
    let (_remain, (seq_25519, seq_mlkem)) = parse_der_container(|i: &[u8], hdr| {
        if hdr.tag() != Tag::Sequence {
            return Err(nom::Err::Error(BerError::InvalidTag));
        }

        // Expect a Seq(Seq, Seq)
        // OID are not checked here, only for variable order
        if let Ok((i, seq_25519)) = parse_seq_tag_bitstring(i) {
            // Ed/X 25519 then MLKEM
            let (i, seq_mlkem) = parse_seq_tag_octetstring(i)?;
            eof(i)?;
            Ok((i, (seq_25519, seq_mlkem)))
        } else if let Ok((i, seq_mlkem)) = parse_seq_tag_octetstring(i) {
            // MLKEM then Ed/X 25519
            let (i, seq_25519) = parse_seq_tag_bitstring(i)?;
            eof(i)?;
            Ok((i, (seq_25519, seq_mlkem)))
        } else {
            return Err(nom::Err::Error(BerError::InvalidTag));
        }
    })(data)?;

    // Parse ML-KEM part
    let public_key_ml = parse_mlkem_encapkey_internal(seq_mlkem)?;

    // Parse X/Ed 25519 part
    let public_key_ecc = parse_openssl_25519_pubkey_internal(seq_25519)?;

    Ok(HybridPublicKey {
        public_key_ecc,
        public_key_ml,
    })
}

// ---- PEM ----

const PUBLIC_TAG: &[u8] = b"PUBLIC KEY";
const PRIVATE_TAG: &[u8] = b"PRIVATE KEY";

/// Parse an OpenSSL Ed25519 or X25519 public key, either in PEM or DER format
pub fn parse_openssl_25519_pubkey(data: &[u8]) -> Result<PublicKey, MLAKeyParserError> {
    if let Ok(pem_data) = pem::parse(data) {
        // First, try as a PEM
        if pem_data.tag().as_bytes() != PUBLIC_TAG {
            return Err(MLAKeyParserError::InvalidPEMTag);
        }
        parse_openssl_25519_pubkey_der(pem_data.contents())
    } else {
        // Fallback to DER format
        parse_openssl_25519_pubkey_der(data)
    }
}

/// Parse an OpenSSL Ed25519 or X25519 private key, either in PEM or DER format
pub fn parse_openssl_25519_privkey(data: &[u8]) -> Result<StaticSecret, MLAKeyParserError> {
    if let Ok(pem_data) = pem::parse(data) {
        // First, try as a PEM
        if pem_data.tag().as_bytes() != PRIVATE_TAG {
            return Err(MLAKeyParserError::InvalidPEMTag);
        }
        parse_openssl_25519_privkey_der(pem_data.contents())
    } else {
        // Fallback to DER format
        parse_openssl_25519_privkey_der(data)
    }
}

/// Parse several contiguous OpenSSL Ed25519 org X25519 public keys in PEM format
pub fn parse_openssl_25519_pubkeys_pem_many(
    data: &[u8],
) -> Result<Vec<PublicKey>, MLAKeyParserError> {
    let mut output = Vec::new();
    for pem_data in pem::parse_many(data)? {
        if pem_data.tag().as_bytes() != PUBLIC_TAG {
            return Err(MLAKeyParserError::InvalidPEMTag);
        }
        output.push(parse_openssl_25519_pubkey_der(pem_data.contents())?);
    }
    Ok(output)
}

/// Parse an MLA private key, either in PEM or DER format
pub fn parse_mlakey_privkey(data: &[u8]) -> Result<HybridPrivateKey, MLAKeyParserError> {
    if let Ok(pem_data) = pem::parse(data) {
        // First, try as a PEM
        if pem_data.tag().as_bytes() != PRIVATE_TAG {
            return Err(MLAKeyParserError::InvalidPEMTag);
        }
        parse_mlakey_privkey_der(pem_data.contents())
    } else {
        // Fallback to DER format
        parse_mlakey_privkey_der(data)
    }
}

/// Parse an MLA public key, either in PEM or DER format
pub fn parse_mlakey_pubkey(data: &[u8]) -> Result<HybridPublicKey, MLAKeyParserError> {
    if let Ok(pem_data) = pem::parse(data) {
        // First, try as a PEM
        if pem_data.tag().as_bytes() != PUBLIC_TAG {
            return Err(MLAKeyParserError::InvalidPEMTag);
        }
        parse_mlakey_pubkey_der(pem_data.contents())
    } else {
        // Fallback to DER format
        parse_mlakey_pubkey_der(data)
    }
}

/// Parse several contiguous MLA public keys in PEM format
pub fn parse_mlakey_pubkeys_pem_many(
    data: &[u8],
) -> Result<Vec<HybridPublicKey>, MLAKeyParserError> {
    let mut output = Vec::new();
    for pem_data in pem::parse_many(data)? {
        if pem_data.tag().as_bytes() != PUBLIC_TAG {
            return Err(MLAKeyParserError::InvalidPEMTag);
        }
        output.push(parse_mlakey_pubkey_der(pem_data.contents())?);
    }
    Ok(output)
}

// ---- Strict Export ----

// This is done with constant data instead of real DER building, as the format
// is strict and key size are constant

const PRIV_KEY_PREFIX: &[u8] = b"\x30\x2e\x02\x01\x00\x30\x05\x06\x03\x2b\x65\x6e\x04\x22\x04\x20";
const PUB_KEY_PREFIX: &[u8] = b"\x30\x2a\x30\x05\x06\x03\x2b\x65\x6e\x03\x21\x00";
const PRIV_KEY_TAG: &str = "PRIVATE KEY";
const PUB_KEY_TAG: &str = "PUBLIC KEY";

pub struct KeyPair {
    pub public_der: [u8; PUB_KEY_PREFIX.len() + ECC_PUBKEY_SIZE],
    pub private_der: [u8; PRIV_KEY_PREFIX.len() + ECC_PRIVKEY_SIZE],
}

impl KeyPair {
    pub fn public_as_pem(&self) -> String {
        let out = pem::Pem::new(PUB_KEY_TAG, self.public_der.to_vec());
        pem::encode(&out)
    }

    pub fn private_as_pem(&self) -> String {
        let out = pem::Pem::new(PRIV_KEY_TAG, self.private_der.to_vec());
        pem::encode(&out)
    }
}

/// Generate a keypair, in DER format
///
/// Keypairs can later be converted to PEM using `public_as_pem`, `private_as_pem`
pub fn generate_keypair<T>(csprng: &mut T) -> Option<KeyPair>
where
    T: RngCore + CryptoRng,
{
    // Get the seed
    let mut private = [0u8; ECC_PRIVKEY_SIZE];
    csprng.fill_bytes(&mut private);

    // Get the corresponding public key
    let priv_key = StaticSecret::from(private);
    let pubkey = PublicKey::from(&priv_key);

    // Get the public data bytes

    let public = pubkey.as_bytes();

    let mut private_der = [0u8; PRIV_KEY_PREFIX.len() + ECC_PRIVKEY_SIZE];
    private_der[..PRIV_KEY_PREFIX.len()].copy_from_slice(PRIV_KEY_PREFIX);
    private_der[PRIV_KEY_PREFIX.len()..].copy_from_slice(&private);

    let mut public_der = [0u8; PUB_KEY_PREFIX.len() + ECC_PUBKEY_SIZE];
    public_der[..PUB_KEY_PREFIX.len()].copy_from_slice(PUB_KEY_PREFIX);
    public_der[PUB_KEY_PREFIX.len()..].copy_from_slice(&public[..]);

    Some(KeyPair {
        public_der,
        private_der,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kem::{Decapsulate, Encapsulate};
    use rand::rngs::OsRng;
    use x25519_dalek::PublicKey;

    static MLA_DER_PRIV: &[u8] = include_bytes!("../../samples/test_mlakey.der");
    static MLA_DER_PUB: &[u8] = include_bytes!("../../samples/test_mlakey_pub.der");
    static MLA_PEM_PRIV: &[u8] = include_bytes!("../../samples/test_mlakey.pem");
    static MLA_PEM_PUB: &[u8] = include_bytes!("../../samples/test_mlakey_pub.pem");

    // Samples, generated by:
    // openssl genpkey -algorithm x25519 -outform DER -out test_x25519.der
    static X_DER_PRIV: &[u8] = include_bytes!("../../samples/test_x25519.der");
    // openssl pkey -outform DER -pubout -in test_x25519.der -inform DER -out test_x25519_pub.der
    static X_DER_PUB: &[u8] = include_bytes!("../../samples/test_x25519_pub.der");

    // openssl genpkey -algorithm ed25519 -outform DER -out test_ed25519.der
    static ED_DER_PRIV: &[u8] = include_bytes!("../../samples/test_ed25519.der");
    // openssl pkey -outform DER -pubout -in test_ed25519.der -inform DER -out test_ed25519_pub.der
    static ED_DER_PUB: &[u8] = include_bytes!("../../samples/test_ed25519_pub.der");

    // openssl pkey -in test_ed25519_pub.der -inform DER -pubin -out test_ed25519_pub.pem -outform PEM -pubout
    static PEM_PUB: &[u8] = include_bytes!("../../samples/test_ed25519_pub.pem");
    // openssl pkey -in test_ed25519.der -inform DER -out test_ed25519.pem -outform PEM
    static PEM_PRIV: &[u8] = include_bytes!("../../samples/test_ed25519.pem");

    // Many[0] is PEM_PUB
    static PEM_PUB_MANY: &[u8] = include_bytes!("../../samples/test_25519_pub_many.pem");

    #[test]
    fn parse_and_check_ed_pubkeys_der() {
        let priv_key = parse_openssl_25519_privkey_der(ED_DER_PRIV).unwrap();
        let pub_key = parse_openssl_25519_pubkey_der(ED_DER_PUB).unwrap();
        let computed_pub_key = PublicKey::from(&priv_key);
        assert_eq!(pub_key.as_bytes().len(), ECC_PUBKEY_SIZE);
        assert_eq!(priv_key.to_bytes().len(), ECC_PRIVKEY_SIZE);
        assert_eq!(computed_pub_key.as_bytes(), pub_key.as_bytes());
    }

    #[test]
    fn parse_and_check_x_pubkeys_der() {
        let priv_key = parse_openssl_25519_privkey_der(X_DER_PRIV).unwrap();
        let pub_key = parse_openssl_25519_pubkey_der(X_DER_PUB).unwrap();
        let computed_pub_key = PublicKey::from(&priv_key);
        assert_eq!(pub_key.as_bytes().len(), ECC_PUBKEY_SIZE);
        assert_eq!(priv_key.to_bytes().len(), ECC_PRIVKEY_SIZE);
        assert_eq!(computed_pub_key.as_bytes(), pub_key.as_bytes());
    }

    #[test]
    fn parse_and_check_pubkeys_multi_format() {
        let pub_key_pem = parse_openssl_25519_pubkey(PEM_PUB).unwrap();
        let pub_key_der = parse_openssl_25519_pubkey(ED_DER_PUB).unwrap();
        assert_eq!(pub_key_der.as_bytes().len(), ECC_PUBKEY_SIZE);
        assert_eq!(pub_key_der.as_bytes(), pub_key_pem.as_bytes());
        let priv_key_pem = parse_openssl_25519_privkey(PEM_PRIV).unwrap();
        let priv_key_der = parse_openssl_25519_privkey(ED_DER_PRIV).unwrap();
        assert_eq!(priv_key_der.to_bytes().len(), ECC_PRIVKEY_SIZE);
        assert_eq!(priv_key_der.to_bytes(), priv_key_pem.to_bytes());
    }

    #[test]
    fn parse_many_pubkeys() {
        let pub_keys_pem = parse_openssl_25519_pubkeys_pem_many(PEM_PUB).unwrap();
        assert_eq!(pub_keys_pem.len(), 1);
        let pub_key_der = parse_openssl_25519_pubkey(ED_DER_PUB).unwrap();
        assert_eq!(pub_key_der.as_bytes().len(), ECC_PUBKEY_SIZE);
        assert_eq!(pub_key_der.as_bytes(), pub_keys_pem[0].as_bytes());

        let pub_keys_pem = parse_openssl_25519_pubkeys_pem_many(PEM_PUB_MANY).unwrap();
        assert_eq!(pub_keys_pem.len(), 3);
        assert_eq!(pub_key_der.as_bytes(), pub_keys_pem[0].as_bytes());
        assert_ne!(pub_key_der.as_bytes(), pub_keys_pem[1].as_bytes());

        let pub_x_key_der = parse_openssl_25519_pubkey(X_DER_PUB).unwrap();
        assert_eq!(pub_x_key_der.as_bytes(), pub_keys_pem[2].as_bytes());
    }

    #[test]
    fn exports() {
        let mut csprng = OsRng {};
        let keypair = generate_keypair(&mut csprng).unwrap();

        let priv_key = parse_openssl_25519_privkey_der(&keypair.private_der).unwrap();
        let pub_key = parse_openssl_25519_pubkey_der(&keypair.public_der).unwrap();
        let computed_pub_key = PublicKey::from(&priv_key);
        assert_eq!(pub_key.as_bytes().len(), ECC_PUBKEY_SIZE);
        assert_eq!(priv_key.to_bytes().len(), ECC_PRIVKEY_SIZE);
        assert_eq!(computed_pub_key.as_bytes(), pub_key.as_bytes());

        let pub_pem_key = keypair.public_as_pem();
        assert_eq!(
            parse_openssl_25519_pubkey(pub_pem_key.as_bytes())
                .unwrap()
                .as_bytes(),
            pub_key.as_bytes()
        );
        let priv_pem_key = keypair.private_as_pem();
        assert_eq!(
            &parse_openssl_25519_privkey(priv_pem_key.as_bytes())
                .unwrap()
                .to_bytes(),
            &priv_key.to_bytes()
        );
    }

    /// Parse a DER public & private key, then check the keys correspond
    #[test]
    fn parse_and_check_mlakey_der() {
        let priv_key = parse_mlakey_privkey_der(MLA_DER_PRIV).unwrap();
        let pub_key = parse_mlakey_pubkey_der(MLA_DER_PUB).unwrap();

        // Check the public ECC key rebuilt from the private ECC key is the expected one
        let computed_ecc_pubkey = PublicKey::from(&priv_key.private_key_ecc);
        assert_eq!(pub_key.public_key_ecc.as_bytes().len(), ECC_PUBKEY_SIZE);
        assert_eq!(priv_key.private_key_ecc.as_bytes().len(), ECC_PRIVKEY_SIZE);
        assert_eq!(
            pub_key.public_key_ecc.as_bytes(),
            computed_ecc_pubkey.as_bytes()
        );

        // Check the public ML-KEM key correspond to the private one
        assert_eq!(
            pub_key.public_key_ml.as_bytes().len(),
            MLKEM_1024_PUBKEY_SIZE
        );
        let mut rng = OsRng {};
        let (encap, key) = pub_key.public_key_ml.encapsulate(&mut rng).unwrap();
        let key_decap = priv_key.private_key_ml.decapsulate(&encap).unwrap();
        assert_eq!(key, key_decap);
    }

    /// Parse the same public key in DER and PEM format
    #[test]
    fn parse_pub_der_pem() {
        let pub_key_der = parse_mlakey_pubkey_der(MLA_DER_PUB).unwrap();
        let pub_key_pem = parse_mlakey_pubkey(MLA_PEM_PUB).unwrap();
        assert_eq!(pub_key_der.public_key_ecc.as_bytes().len(), ECC_PUBKEY_SIZE);
        assert_eq!(
            pub_key_der.public_key_ml.as_bytes().len(),
            MLKEM_1024_PUBKEY_SIZE
        );
        assert_eq!(
            pub_key_der.public_key_ecc.as_bytes(),
            pub_key_pem.public_key_ecc.as_bytes()
        );
        assert_eq!(
            pub_key_der.public_key_ml.as_bytes(),
            pub_key_pem.public_key_ml.as_bytes()
        );
    }

    /// Parse the same private key in DER and PEM format
    #[test]
    fn parse_priv_der_pem() {
        let priv_key_der = parse_mlakey_privkey_der(MLA_DER_PRIV).unwrap();
        let priv_key_pem = parse_mlakey_privkey(MLA_PEM_PRIV).unwrap();
        assert_eq!(
            priv_key_der.private_key_ecc.as_bytes().len(),
            ECC_PRIVKEY_SIZE
        );
        assert_eq!(
            priv_key_der.private_key_ml.as_bytes().len(),
            MLKEM_1024_PRIVKEY_SIZE
        );
        assert_eq!(
            priv_key_der.private_key_ecc.as_bytes(),
            priv_key_pem.private_key_ecc.as_bytes()
        );
        assert_eq!(
            priv_key_der.private_key_ml.as_bytes(),
            priv_key_pem.private_key_ml.as_bytes()
        );
    }
}
