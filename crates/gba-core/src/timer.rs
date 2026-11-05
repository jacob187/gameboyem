//! The four timers (0x0400_0100..0x0400_0110), prescalers and cascade. Owned by
//! P1-SYS. Overflows are `Event::TimerOverflow(n)` on the scheduler, computed from
//! the reload value and prescaler (count-up timers are advanced by the previous
//! timer's overflow). On an overflow of timer 0 or 1 the bus also tells the APU
//! (`Apu::on_timer_overflow`), which may request a FIFO DMA.

#[derive(Debug, Clone, Default)]
pub struct Timers {
    _reserved: (),
}
