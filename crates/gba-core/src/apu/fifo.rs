//! Direct Sound FIFO A/B (GBATEK "GBA Sound Channel A and B - DMA Sound").

/// 8 words = 32 bytes of signed 8-bit samples.
const CAPACITY: usize = 32;

/// A DMA refill is requested when the FIFO holds this many bytes or fewer
/// (4 words; GBATEK: "DMA request when the FIFO has 4 words left").
pub(super) const REFILL_LEVEL: usize = 16;

#[derive(Clone, Debug, Default)]
pub(super) struct Fifo {
    buf: [i8; CAPACITY],
    rd: usize,
    len: usize,
    /// The sample the DAC currently outputs: the last one popped. It holds when
    /// the FIFO runs dry (an approximation; GBATEK does not say).
    pub sample: i8,
}

impl Fifo {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.len
    }

    /// A 32-bit write: four samples, the lowest byte first. Bytes that do not
    /// fit in a full FIFO are lost.
    pub fn push_word(&mut self, v: u32) {
        for b in v.to_le_bytes() {
            if self.len == CAPACITY {
                return;
            }
            self.buf[(self.rd + self.len) % CAPACITY] = b as i8;
            self.len += 1;
        }
    }

    /// A timer overflow: output the next sample, if any. Returns `true` if the
    /// output changed.
    pub fn pop(&mut self) -> bool {
        if self.len == 0 {
            return false;
        }
        let before = self.sample;
        self.sample = self.buf[self.rd];
        self.rd = (self.rd + 1) % CAPACITY;
        self.len -= 1;
        self.sample != before
    }

    pub fn wants_refill(&self) -> bool {
        self.len <= REFILL_LEVEL
    }

    /// SOUNDCNT_H reset bit: empty the FIFO and silence the output.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_push_lowest_byte_first_and_pop_in_order() {
        let mut f = Fifo::default();
        f.push_word(0x80_7F_01_FF);
        assert_eq!(f.len(), 4);
        let mut got = vec![];
        while f.len() > 0 {
            f.pop();
            got.push(f.sample);
        }
        assert_eq!(got, [-1, 1, 127, -128]);
    }

    #[test]
    fn full_fifo_drops_extra_bytes() {
        let mut f = Fifo::default();
        for i in 0..9u32 {
            f.push_word(i * 0x0101_0101);
        }
        assert_eq!(f.len(), 32);
        for _ in 0..8 {
            for _ in 0..4 {
                f.pop();
            }
        }
        assert_eq!(
            f.sample, 0x07,
            "the ninth word was dropped, the eighth is last"
        );
    }

    #[test]
    fn refill_is_wanted_at_sixteen_bytes_or_fewer() {
        let mut f = Fifo::default();
        for _ in 0..8 {
            f.push_word(0x0101_0101);
        }
        assert!(!f.wants_refill());
        f.pop();
        assert_eq!(f.len(), 31);
        for _ in 0..15 {
            f.pop();
        }
        assert_eq!(f.len(), 16);
        assert!(f.wants_refill(), "16 bytes is the threshold");
    }

    #[test]
    fn empty_pop_holds_the_last_sample() {
        let mut f = Fifo::default();
        f.push_word(0x2222_2222);
        for _ in 0..4 {
            assert!(f.pop() || f.sample == 0x22);
        }
        assert_eq!(f.sample, 0x22);
        assert!(!f.pop(), "nothing to pop");
        assert_eq!(f.sample, 0x22);
        assert!(f.wants_refill(), "an empty FIFO still asks for data");
    }

    #[test]
    fn reset_empties_and_silences() {
        let mut f = Fifo::default();
        f.push_word(0x7F7F_7F7F);
        f.pop();
        f.reset();
        assert_eq!((f.len(), f.sample), (0, 0));
    }
}
