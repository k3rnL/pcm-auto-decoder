/// IEC-61937 preamble words (big-endian)
const PA_SYNC: u16 = 0xF872;
const PB_SYNC: u16 = 0x4E1F;

const DEFAULT_CHUNK_FRAMES: usize = 2048;
const DEFAULT_DET_WINDOW_CHUNKS: usize = 64;

// Pc (16-bit) bit layout:
//  [6:0]   data_type
//  [7]     error
//  [12:8]  type-dependent info
//  [15:13] stream number
pub const PC_TYPE_MASK: u16 = 0x007F; // bits 0..=6
pub const PC_ERR_MASK:  u16 = 0x0080; // bit 7
pub const PC_INFO_MASK: u16 = 0x1F00; // bits 8..=12
pub const PC_STRM_MASK: u16 = 0xE000; // bits 13..=15

pub const PC_TYPE_SHIFT: u8 = 0;
pub const PC_ERR_SHIFT:  u8 = 7;
pub const PC_INFO_SHIFT: u8 = 8;
pub const PC_STRM_SHIFT: u8 = 13;

#[repr(u8)]
pub enum StreamType {
    Ac3 = 0x01,
    EAc3 = 0x15,
    // … add more as needed
    Unknown(u8),
}

impl From<u8> for StreamType {
    fn from(value: u8) -> Self {
        match value {
            0x01 => StreamType::Ac3,
            0x15 => StreamType::EAc3,
            other => StreamType::Unknown(other),
        }
    }
}

pub struct Iec61937Preamble {
    pub stream_type: StreamType, // Pc[6:0]
    pub error: bool,             // Pc[7]
    pub info: u8,                // Pc[12:8] (type-dependent width)
    pub stream_number: u8,       // Pc[15:13]
    pub length_code: u16,        // raw Pd (do not pre-convert)
}

impl Iec61937Preamble {
    pub fn payload_bytes(&self) -> Option<usize> {
        match self.stream_type {
            StreamType::Ac3 => Some((self.length_code as usize) / 8), // Pd in bits → bytes
            StreamType::EAc3 => Some(self.length_code as usize),      // Pd already in bytes
            StreamType::Unknown(_) => None,
        }
    }
}

pub struct Iec61937Detector {}
impl Iec61937Detector {
    pub fn new() -> Self {
        Self {}
    }

    pub fn find_preamble(bytes: &[u8]) -> Option<Iec61937Preamble> {
        if bytes.len() < 8 { return None; }

        // Read 16-bit words as little-endian (least-significant byte first)
        let pa = u16::from_le_bytes([bytes[0], bytes[1]]);
        let pb = u16::from_le_bytes([bytes[2], bytes[3]]);
        if pa != PA_SYNC || pb != PB_SYNC {
            return None;
        }

        let pc = u16::from_le_bytes([bytes[4], bytes[5]]);
        let pd = u16::from_le_bytes([bytes[6], bytes[7]]);

        let data_type    = ((pc & PC_TYPE_MASK) >> PC_TYPE_SHIFT) as u8;
        let error        = ((pc & PC_ERR_MASK)  >> PC_ERR_SHIFT)  != 0;
        let info         = ((pc & PC_INFO_MASK) >> PC_INFO_SHIFT) as u8;      // width is type-dependent
        let stream_num   = ((pc & PC_STRM_MASK) >> PC_STRM_SHIFT) as u8;

        Some(Iec61937Preamble {
            stream_type: data_type.into(),
            error,
            info,
            stream_number: stream_num,
            length_code: pd,
        })
    }
}
