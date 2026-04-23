pub trait AudioOutput: Send + 'static {
    fn push_opus(&mut self, payload: &[u8]);
    fn new() -> Self;
}