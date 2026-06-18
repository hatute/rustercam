#[derive(Clone)]
pub struct Frame {
    pub data: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Default)]
pub struct RenderFrame {
    pub chars: Vec<String>,
    pub colors: Option<Vec<Vec<(u8, u8, u8)>>>,
}
