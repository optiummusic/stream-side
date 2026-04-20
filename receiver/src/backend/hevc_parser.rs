use std::ops::Range;

/// Читает HEVC RBSP побитово (с удалением emulation prevention bytes 0x000003)
pub struct RbspReader<'a> {
    data: &'a [u8],
    bit_pos: usize, // глобальная позиция в битах (по очищенным данным)
    cleaned: Vec<u8>,
}

impl<'a> RbspReader<'a> {
    pub fn new(nal_data: &'a [u8]) -> Self {
        // Убираем emulation_prevention_three_byte (0x00 0x00 0x03 → 0x00 0x00)
        let mut cleaned = Vec::with_capacity(nal_data.len());
        let mut i = 0;
        while i < nal_data.len() {
            if i + 2 < nal_data.len()
                && nal_data[i] == 0x00
                && nal_data[i + 1] == 0x00
                && nal_data[i + 2] == 0x03
            {
                cleaned.push(0x00);
                cleaned.push(0x00);
                i += 3; // пропускаем 0x03
            } else {
                cleaned.push(nal_data[i]);
                i += 1;
            }
        }
        Self { data: nal_data, bit_pos: 0, cleaned }
    }

    pub fn read_bit(&mut self) -> u32 {
        let byte = self.bit_pos / 8;
        let bit  = 7 - (self.bit_pos % 8);
        self.bit_pos += 1;
        if byte < self.cleaned.len() {
            ((self.cleaned[byte] >> bit) & 1) as u32
        } else {
            0
        }
    }

    pub fn read_bits(&mut self, n: usize) -> u32 {
        let mut val = 0u32;
        for _ in 0..n { val = (val << 1) | self.read_bit(); }
        val
    }

    /// ue(v) — Exp-Golomb unsigned
    pub fn read_ue(&mut self) -> u32 {
        let mut leading = 0;
        while self.read_bit() == 0 { leading += 1; }
        if leading == 0 { return 0; }
        let suffix = self.read_bits(leading);
        (1 << leading) - 1 + suffix
    }

    /// Текущая позиция в битах — нужна чтобы запомнить оффсет poc_lsb
    pub fn current_bit_pos(&self) -> usize { self.bit_pos }
}

/// Всё что нам нужно знать из SPS для concealment
#[derive(Clone)]
pub struct HevcSpsInfo {
    pub log2_max_poc_lsb: u8,  // log2_max_pic_order_cnt_lsb_minus4 + 4, обычно 8
}

/// Результат парсинга первого SliceTrailing
pub struct SliceHeaderInfo {
    pub poc_lsb: u32,
    pub poc_lsb_bit_offset: usize, // бит-оффсет внутри NALU (после 2-байт NAL header)
    pub poc_lsb_len: u8,
    pub rps_bit_offset: usize,
    pub rps_original_bit_len: usize,
}

impl SliceHeaderInfo {
    /// Парсим slice header, находим poc_lsb и его бит-позицию.
    /// `nalu` — сырые байты НАЛУ БЕЗ start code (0x000001).
    /// `sps` — параметры из SPS (нужен только log2_max_poc_lsb).
    pub fn parse(nalu: &[u8], sps: &HevcSpsInfo) -> Option<Self> {
        if nalu.len() < 3 { return None; }

        // Первые 2 байта — NAL unit header, пропускаем
        let mut r = RbspReader::new(&nalu[2..]);

        // first_slice_segment_in_pic_flag
        let first_slice = r.read_bit();

        // nal_unit_type из заголовка
        let nal_type = (nalu[0] >> 1) & 0x3F;
        let is_irap = nal_type >= 16 && nal_type <= 23;

        if is_irap {
            r.read_bit(); // no_output_of_prior_pics_flag
        }

        r.read_ue(); // slice_pic_parameter_set_id

        if first_slice == 0 {
            r.read_bit(); // dependent_slice_segment_flag
            // slice_segment_address — нужен log2(PicSizeInCtbsY) бит
            // Для простоты пропускаем через read_ue (приближение)
            r.read_ue();
        }

        // slice_type ue(v): 0=B, 1=P, 2=I
        let slice_type = r.read_ue();

        // pic_output_flag (если output_flag_present_flag в PPS = 1, обычно 0)
        // Пропускаем — без PPS контекста предполагаем 0

        // Запоминаем позицию прямо ПЕРЕД pic_order_cnt_lsb
        let poc_bit_offset = r.current_bit_pos();
        let poc_lsb = if slice_type != 2 {
            r.read_bits(sps.log2_max_poc_lsb as usize)
        } else {
            0 // IDR всегда poc=0
        };

        // После чтения poc_lsb:
        let rps_sps_flag = r.read_bit();
        let rps_bit_offset = r.current_bit_pos() - 1;

        // Измеряем сколько бит занимает RPS чтобы знать сколько можно перезаписать
        let rps_original_bit_len = if rps_sps_flag == 1 {
            // short_term_ref_pic_set_idx — ue(v), читаем и считаем
            let before = r.current_bit_pos();
            r.read_ue();
            r.current_bit_pos() - before + 1 // +1 за sps_flag
        } else {
            // inline RPS: считаем num_neg + num_pos entries
            let before = r.current_bit_pos(); // уже после sps_flag
            let num_neg = r.read_ue();
            let num_pos = r.read_ue();
            for _ in 0..num_neg { r.read_ue(); r.read_bit(); } // delta_poc + used_flag
            for _ in 0..num_pos { r.read_ue(); r.read_bit(); }
            r.current_bit_pos() - before + 1 // +1 за sps_flag
        };

        Some(Self {
            poc_lsb,
            poc_lsb_bit_offset: poc_bit_offset,
            poc_lsb_len: sps.log2_max_poc_lsb,
            rps_bit_offset,
            rps_original_bit_len,
        })
    }
}

/// Шаблон skip P-frame, патчим только poc_lsb
#[derive(Clone)]
pub struct SkipFrameTemplate {
    pub data: Vec<u8>,
    pub poc_lsb_bit_offset: usize,
    pub poc_lsb_len: u8,
    pub max_poc_lsb: u32,
    pub rps_bit_offset: usize,
    pub rps_original_bit_len: usize,
}

impl SkipFrameTemplate {
    pub fn from_real_frame(nalu: &[u8], info: &SliceHeaderInfo) -> Self {
        Self {
            data: nalu.to_vec(),
            poc_lsb_bit_offset: info.poc_lsb_bit_offset,
            poc_lsb_len: info.poc_lsb_len,
            max_poc_lsb: 1 << info.poc_lsb_len,
            rps_bit_offset: info.rps_bit_offset,
            rps_original_bit_len: info.rps_original_bit_len,
        }
    }

    /// Патчим poc_lsb и возвращаем готовый кадр
    pub fn make_frame(&self, poc_lsb: u32) -> Vec<u8> {
        let mut frame = Vec::with_capacity(4 + self.data.len());
        frame.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        frame.extend_from_slice(&self.data);

        let poc = poc_lsb & (self.max_poc_lsb - 1);
        write_bits(&mut frame[4 + 2..], self.poc_lsb_bit_offset, self.poc_lsb_len as usize, poc);

        // RPS: ссылаемся на предыдущий кадр (delta_poc = -1)
        // Кодировка:
        //   sps_flag          = 0        → 1 бит:  0
        //   num_negative_pics = ue(1)    → 3 бита: 010
        //   delta_poc_s0_minus1[0] = ue(0) → 1 бит: 1   (delta = -(0+1) = -1)
        //   used_by_curr_pic_s0_flag[0] = 1 → 1 бит: 1
        //   num_positive_pics = ue(0)    → 1 бит:  1
        // Итого 7 бит
        //
        // Оригинальный RPS нормального P-кадра тоже ~7+ бит (1 ref),
        // поэтому перезапись безопасна.
        let rps_off = self.rps_bit_offset;

        if self.rps_original_bit_len >= 7 {
            // Freeze frame: skip-макроблоки скопируют предыдущий кадр
            write_bits(&mut frame[4 + 2..], rps_off,     1, 0b0);   // sps_flag=0
            write_bits(&mut frame[4 + 2..], rps_off + 1, 3, 0b010); // num_negative=ue(1)
            write_bits(&mut frame[4 + 2..], rps_off + 4, 1, 0b1);   // delta_poc_s0_minus1=ue(0)
            write_bits(&mut frame[4 + 2..], rps_off + 5, 1, 0b1);   // used_by_curr_pic_s0_flag=1
            write_bits(&mut frame[4 + 2..], rps_off + 6, 1, 0b1);   // num_positive=ue(0)
        } else {
            // Fallback: пустой RPS (старое поведение) если шаблон слишком короткий
            write_bits(&mut frame[4 + 2..], rps_off,     1, 0);
            write_bits(&mut frame[4 + 2..], rps_off + 1, 1, 1);
            write_bits(&mut frame[4 + 2..], rps_off + 2, 1, 1);
        }

        frame
    }

    pub(crate) fn parse_sps_poc_bits(nalu: &[u8]) -> Option<u8> {
        if nalu.len() < 3 { return None; }
        // Пропускаем 2 байта NAL header
        let mut r = RbspReader::new(&nalu[2..]);
        
        r.read_bits(4); // sps_video_parameter_set_id
        let max_sub_layers = r.read_bits(3); // sps_max_sub_layers_minus1
        r.read_bit(); // sps_temporal_id_nesting_flag
        
        // profile_tier_level — фиксированный размер зависит от max_sub_layers
        Self::skip_profile_tier_level(&mut r, true, max_sub_layers);
        
        r.read_ue(); // sps_seq_parameter_set_id
        let chroma_format = r.read_ue();
        if chroma_format == 3 {
            r.read_bit(); // separate_colour_plane_flag
        }
        r.read_ue(); // pic_width_in_luma_samples
        r.read_ue(); // pic_height_in_luma_samples
        
        let conformance_window = r.read_bit();
        if conformance_window == 1 {
            r.read_ue(); r.read_ue(); r.read_ue(); r.read_ue();
        }
        
        r.read_ue(); // bit_depth_luma_minus8
        r.read_ue(); // bit_depth_chroma_minus8
        
        // Вот оно!
        let log2_max = r.read_ue();
        Some(log2_max as u8)
    }

    fn skip_profile_tier_level(r: &mut RbspReader, profile_present: bool, max_sub_layers: u32) {
        if profile_present {
            r.read_bits(2);  // general_profile_space
            r.read_bit();    // general_tier_flag
            r.read_bits(5);  // general_profile_idc
            r.read_bits(32); // general_profile_compatibility_flag[32]
            r.read_bit();    // general_progressive_source_flag
            r.read_bit();    // general_interlaced_source_flag
            r.read_bit();    // general_non_packed_constraint_flag
            r.read_bit();    // general_frame_only_constraint_flag
            r.read_bits(32); // general_reserved_zero_44bits (часть)
            r.read_bits(12);
            r.read_bits(8);  // general_level_idc
        }
        
        let mut sub_layer_profile = vec![false; max_sub_layers as usize];
        let mut sub_layer_level   = vec![false; max_sub_layers as usize];
        for i in 0..max_sub_layers as usize {
            sub_layer_profile[i] = r.read_bit() == 1;
            sub_layer_level[i]   = r.read_bit() == 1;
        }
        for i in 0..max_sub_layers as usize {
            if sub_layer_profile[i] {
                r.read_bits(2); r.read_bit(); r.read_bits(5);
                r.read_bits(32); r.read_bits(4); r.read_bits(32); r.read_bits(12);
                r.read_bits(8);
            }
            if sub_layer_level[i] {
                r.read_bits(8);
            }
        }
    }
}

/// Записывает `len` бит значения `val` начиная с `bit_offset` в `buf`
fn write_bits(buf: &mut [u8], bit_offset: usize, len: usize, val: u32) {
    for i in 0..len {
        let bit = (val >> (len - 1 - i)) & 1;
        let pos = bit_offset + i;
        let byte = pos / 8;
        let shift = 7 - (pos % 8);
        if byte < buf.len() {
            buf[byte] = (buf[byte] & !(1 << shift)) | ((bit as u8) << shift);
        }
    }
}