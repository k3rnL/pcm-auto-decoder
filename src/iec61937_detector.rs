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
pub const PC_ERR_MASK: u16 = 0x0080; // bit 7
pub const PC_INFO_MASK: u16 = 0x1F00; // bits 8..=12
pub const PC_STRM_MASK: u16 = 0xE000; // bits 13..=15

pub const PC_TYPE_SHIFT: u8 = 0;
pub const PC_ERR_SHIFT: u8 = 7;
pub const PC_INFO_SHIFT: u8 = 8;
pub const PC_STRM_SHIFT: u8 = 13;

#[repr(u8)]
#[derive(Debug, PartialEq)]
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

#[derive(Debug)]
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
        if bytes.len() < 8 {
            return None;
        }

        // IEC61937 sync words, little endian
        const PA_SYNC_LE: [u8; 2] = [0x72, 0xF8]; // 0xF872
        const PB_SYNC_LE: [u8; 2] = [0x1F, 0x4E]; // 0x4E1F

        // scan up to len - 7 to have room for the whole header
        for i in 0..=bytes.len().saturating_sub(8) {
            if bytes[i..i + 2] == PA_SYNC_LE && bytes[i + 2..i + 4] == PB_SYNC_LE {
                let pc = u16::from_le_bytes([bytes[i + 4], bytes[i + 5]]);
                let pd = u16::from_le_bytes([bytes[i + 6], bytes[i + 7]]);

                let data_type = ((pc & PC_TYPE_MASK) >> PC_TYPE_SHIFT) as u8;
                let error = ((pc & PC_ERR_MASK) >> PC_ERR_SHIFT) != 0;
                let info = ((pc & PC_INFO_MASK) >> PC_INFO_SHIFT) as u8;
                let stream_num = ((pc & PC_STRM_MASK) >> PC_STRM_SHIFT) as u8;

                return Some(Iec61937Preamble {
                    stream_type: data_type.into(),
                    error,
                    info,
                    stream_number: stream_num,
                    length_code: pd,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iec61937_detector::StreamType::Ac3;
    use anyhow::Context;
    use base64::Engine;

    #[test]
    fn can_detect_ac3() -> Result<(), String> {
        use base64::prelude::*;

        let data = "cvgfTgEAAFB3Cy/EQCSEL8Ir+wFeh+cbn++uPvP5z+e+of8ywdx7ik+c2JTYbyuIq+IY5D1zdQaI2Q+PL6YsxSS8F57bmIWYcRobLiMwSRZpP/t2IgJAygnO2wRzVZkgwR4DhBIMUKsB0qMuUkqx8SE+gufC6KBGwfdjk4IkwI5hxINUwlDgxUGeYyQickHwwQBD/EIZQCZB0kO7wj0AXQGxw4wCYICKQZTDY0J5QK3BeMM+wo6AzkFmwyECoEDoQVQDCMOwgAOBlQPpAnmAKoF8A8eClIBQgWUDoIKhgGwBVIOEgrOAiAFEA2mCvwCfgTUDU4LQgLmBKoM4AtIAx4EgAyqC4IDdgR2DHoLrgO0BFgMUAvgA/AESA/gC6wAIAQUA6AICAyIB8wDKAgsDPQHwALsCGANPAeIArgI3A3kB4ACLAiMDdAHOAHsCMQOLAckAZwIwA5YBxQBgAkADrQHHAFQCQAOzAb8ASQJBA7EBqwA9AlUDygGmACgCUgPXAagAIAJVA+IBqQAcAlkD5AGeABICYwP1AaEADgJoA/0BnAABAmID9wGRAAIDegMPARgA1gLaAx4BFgDaAvIDOAEYAdICAANKAQwAuAL0AkQB+gCoAvoBUD8cAyDxXnz8tfpUej6rraN09dOpfiW6lq+ppZ2lqOjrK2i2pvuCKve41P2QnfJrc3Ol9L3PJyS4CX1nVXXzjEMArACIDHAAeA0aQXWr1pDIe9YhGJau0qHE3rFBQLNAiAIEAIQOgAQAtADwA0AEwAFADoAEABYAGAOQBCAYgJAA6ABgAKADgAOAAwAFAAgALAAEABYADAAAALAB8ABgAwAA4APgAyABAAMAA0ADgAHgAQAAQAMwAYAAIAKAA8ABgACAAcAAYAGAAUADAACAAKAAQANAAgADwALAA8ALgAmADwAOAAeABoAOAATABAAFAA+ACoAEAAIACgAMAAAAAwAJAAEACwAEgA0ADAAOAAwAAAAAgAEAA0ALAAMACwACgAQADQAKAAEACgABAAqADkAMAA0AD4AMAAmADQAPAASABAANwA8ADwABgAQAA4ANAA6AC0APYAuACwAAAAkACwAJAA0ABAAAgAQAAIAAAAUABAADAAYADAAAAAsABAAOAAwACgAFgANABoAGAA4AAIAIgA2ABAAPgAMADYAFgA+ABAAMAAuAAAADBuCBGmhAAw48UIELLIgBAx8AYAMG8IAACSCABXyAATawwAcA8AACgAIE6AEDBgDiwP0A+gD/8K9U/jXb4P4Tta8B6YHIAYy1JgAr/zT9UBRAEABAAEAOABEAAAC4AAAAKA0gCkAGAAgAAAMAA4AAAAEAOgA0ABwDMALAA8AB4A==";
        let bytes = BASE64_STANDARD
            .decode(data)
            .map_err(move |e| e.to_string())?;

        let preamble = Iec61937Detector::find_preamble(bytes.as_slice());
        assert!(preamble.is_some());
        assert_eq!(preamble.unwrap().stream_type, Ac3);
        Ok(())
    }
}
