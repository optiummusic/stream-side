use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct DpbEntry {
    pub abs_poc: i32,
    pub lsb: u32,
    pub long_term: bool,
    pub is_reference: bool,
    pub output_marked: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ShortTermRps {
    /// Более ранние кадры, чем текущий, в порядке от ближайшего к дальнему.
    pub negative: Vec<RefPic>,
    /// Более поздние кадры, чем текущий, в порядке от ближайшего к дальнему.
    pub positive: Vec<RefPic>,
}

#[derive(Debug, Clone)]
pub struct RefPic {
    /// Абсолютный POC референса.
    pub abs_poc: i32,
    /// Signed delta POC относительно текущего кадра.
    /// negative: -1, -2, ...
    /// positive: +1, +2, ...
    pub delta_poc: i32,
    pub used_by_curr: bool,
    pub is_long_term: bool,
}

impl ShortTermRps {
    pub fn is_empty(&self) -> bool {
        self.negative.is_empty() && self.positive.is_empty()
    }

    pub fn total_refs(&self) -> usize {
        self.negative.len() + self.positive.len()
    }

    pub fn filter_by_dpb(&self, dpb: &[DpbEntry]) -> Self {
        let present: HashSet<i32> = dpb.iter().map(|e| e.abs_poc).collect();

        Self {
            negative: self
                .negative
                .iter()
                .filter(|pic| present.contains(&pic.abs_poc))
                .cloned()
                .collect(),
            positive: self
                .positive
                .iter()
                .filter(|pic| present.contains(&pic.abs_poc))
                .cloned()
                .collect(),
        }
    }

    pub fn encode_inline(&self, w: &mut BitWriter) {
        // short_term_ref_pic_set( ) inline form:
        // inter_ref_pic_set_prediction_flag = 0
        w.write_bit(false);
        w.write_ue(self.negative.len() as u32);
        w.write_ue(self.positive.len() as u32);

        // Negative pics: closest first
        for pic in &self.negative {
            let delta = pic.delta_poc.abs() as u32;
            let minus1 = delta.saturating_sub(1);
            w.write_ue(minus1);
            w.write_bit(pic.used_by_curr);
        }

        // Positive pics: closest first
        for pic in &self.positive {
            let delta = pic.delta_poc.abs() as u32;
            let minus1 = delta.saturating_sub(1);
            w.write_ue(minus1);
            w.write_bit(pic.used_by_curr);
        }
    }
}

#[derive(Debug, Clone)]
pub struct HevcState {
    pub dpb: Vec<DpbEntry>,
    pub last_valid_rps: Option<ShortTermRps>,
    pub prev_poc_msb: i32,
    pub prev_poc_lsb: u32,
    pub max_poc_lsb: u32,
}

impl HevcState {
    pub fn new(log2_max_poc_lsb: u8) -> Self {
        let max_poc_lsb = 1u32 << log2_max_poc_lsb;
        Self {
            dpb: Vec::new(),
            last_valid_rps: None,
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
            max_poc_lsb,
        }
    }

    #[inline]
    pub fn derive_abs_poc(&self, poc_lsb: u32) -> i32 {
        let max = self.max_poc_lsb as i32;
        let half = max / 2;
        let prev_msb = self.prev_poc_msb;
        let prev_lsb = self.prev_poc_lsb as i32;
        let lsb = poc_lsb as i32;

        let poc_msb = if lsb < prev_lsb && (prev_lsb - lsb) >= half {
            prev_msb + max
        } else if lsb > prev_lsb && (lsb - prev_lsb) > half {
            prev_msb - max
        } else {
            prev_msb
        };

        poc_msb + lsb
    }

    pub fn remember_picture(
        &mut self,
        poc_lsb: u32,
        is_reference: bool,
        output_marked: bool,
        long_term: bool,
    ) -> i32 {
        let abs_poc = self.derive_abs_poc(poc_lsb);
        let lsb = poc_lsb & (self.max_poc_lsb - 1);

        self.prev_poc_msb = abs_poc - lsb as i32;
        self.prev_poc_lsb = lsb;

        if is_reference {
            self.dpb.push(DpbEntry {
                abs_poc,
                lsb,
                long_term,
                is_reference,
                output_marked,
            });
            self.prune_dpb();
            self.last_valid_rps = Some(self.build_short_term_rps(abs_poc));
        }

        abs_poc
    }

    pub fn mark_output_and_prune(&mut self, abs_poc: i32) {
        if let Some(entry) = self.dpb.iter_mut().find(|e| e.abs_poc == abs_poc) {
            entry.output_marked = true;
        }
        self.prune_dpb();
    }

    pub fn rebuild_last_valid_rps(&mut self, curr_abs_poc: i32) -> Option<ShortTermRps> {
        let rps = self.build_short_term_rps(curr_abs_poc);
        if rps.is_empty() {
            None
        } else {
            self.last_valid_rps = Some(rps.clone());
            Some(rps)
        }
    }

    pub fn current_concealment_rps(&self, curr_abs_poc: i32) -> Option<ShortTermRps> {
        let rps = if let Some(saved) = &self.last_valid_rps {
            let filtered = saved.filter_by_dpb(&self.dpb);
            if !filtered.is_empty() {
                filtered
            } else {
                self.build_short_term_rps(curr_abs_poc)
            }
        } else {
            self.build_short_term_rps(curr_abs_poc)
        };

        if rps.is_empty() { None } else { Some(rps) }
    }

    pub fn build_short_term_reference_list(
        &self,
        curr_abs_poc: i32,
        max_refs: usize,
    ) -> Vec<i32> {
        let mut refs: Vec<i32> = self
            .dpb
            .iter()
            .filter(|e| e.is_reference && !e.long_term)
            .map(|e| e.abs_poc)
            .filter(|&poc| poc != curr_abs_poc)
            .collect();

        refs.sort_by_key(|&poc| (curr_abs_poc - poc).abs());
        refs.truncate(max_refs);
        refs
    }

    pub fn build_short_term_rps(&self, curr_abs_poc: i32) -> ShortTermRps {
        let mut negative: Vec<&DpbEntry> = self
            .dpb
            .iter()
            .filter(|e| e.is_reference && !e.long_term && e.abs_poc < curr_abs_poc)
            .collect();
        negative.sort_by(|a, b| b.abs_poc.cmp(&a.abs_poc)); // closest first

        let mut positive: Vec<&DpbEntry> = self
            .dpb
            .iter()
            .filter(|e| e.is_reference && !e.long_term && e.abs_poc > curr_abs_poc)
            .collect();
        positive.sort_by(|a, b| a.abs_poc.cmp(&b.abs_poc)); // closest first

        ShortTermRps {
            negative: negative
                .into_iter()
                .map(|e| RefPic {
                    abs_poc: e.abs_poc,
                    delta_poc: e.abs_poc - curr_abs_poc,
                    used_by_curr: true,
                    is_long_term: false,
                })
                .collect(),
            positive: positive
                .into_iter()
                .map(|e| RefPic {
                    abs_poc: e.abs_poc,
                    delta_poc: e.abs_poc - curr_abs_poc,
                    used_by_curr: true,
                    is_long_term: false,
                })
                .collect(),
        }
    }

    pub fn filter_rps_to_dpb(&self, rps: &ShortTermRps) -> ShortTermRps {
        rps.filter_by_dpb(&self.dpb)
    }

    pub fn serialize_inline_short_term_rps(&self, curr_abs_poc: i32) -> Option<Vec<u8>> {
        let rps = self.current_concealment_rps(curr_abs_poc)?;
        let mut w = BitWriter::new();
        rps.encode_inline(&mut w);
        Some(w.into_bytes())
    }

    fn prune_dpb(&mut self) {
        // Keep only reference pictures in the active DPB.
        self.dpb.retain(|e| e.is_reference);

        // Practical guardrail.
        if self.dpb.len() > 16 {
            self.dpb.sort_by_key(|e| e.abs_poc);
            let overflow = self.dpb.len() - 16;
            self.dpb.drain(0..overflow);
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    bit_len: usize,
}

impl BitWriter {
    pub fn new() -> Self {
        Self { bytes: Vec::new(), bit_len: 0 }
    }

    pub fn write_bit(&mut self, bit: bool) {
        let byte_idx = self.bit_len / 8;
        let bit_idx = 7 - (self.bit_len % 8);
        if byte_idx == self.bytes.len() {
            self.bytes.push(0);
        }
        if bit {
            self.bytes[byte_idx] |= 1 << bit_idx;
        }
        self.bit_len += 1;
    }

    pub fn write_bits(&mut self, value: u64, len: usize) {
        for i in (0..len).rev() {
            self.write_bit(((value >> i) & 1) != 0);
        }
    }

    pub fn write_ue(&mut self, value: u32) {
        let code_num = value + 1;
        let num_bits = 32 - code_num.leading_zeros();
        let leading_zeros = (num_bits - 1) as usize;

        for _ in 0..leading_zeros {
            self.write_bit(false);
        }
        self.write_bit(true);

        if leading_zeros > 0 {
            let suffix = code_num ^ (1 << leading_zeros);
            self.write_bits(suffix as u64, leading_zeros);
        }
    }

    pub fn write_se(&mut self, value: i32) {
        let ue_val: u32 = if value > 0 {
            (value as u32) * 2 - 1
        } else {
            (-value as u32) * 2
        };
        self.write_ue(ue_val);
    }

    pub fn bit_len(&self) -> usize {
        self.bit_len
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

fn get_bit(data: &[u8], bit_idx: usize) -> u8 {
    let byte = bit_idx / 8;
    if byte >= data.len() {
        return 0;
    }
    let shift = 7 - (bit_idx % 8);
    (data[byte] >> shift) & 1
}

fn bits_to_bytes(bits: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; (bits.len() + 7) / 8];
    for (i, &bit) in bits.iter().enumerate() {
        if bit != 0 {
            let byte = i / 8;
            let shift = 7 - (i % 8);
            out[byte] |= 1 << shift;
        }
    }
    out
}

fn splice_bits(
    data: &[u8],
    start_bit: usize,
    remove_bits: usize,
    insert_bits: &[u8],
    insert_bit_len: usize,
) -> Vec<u8> {
    let total_bits = data.len() * 8;
    let end_remove = start_bit.saturating_add(remove_bits);

    let mut out_bits =
        Vec::with_capacity(total_bits.saturating_sub(remove_bits).saturating_add(insert_bit_len));

    for i in 0..start_bit {
        out_bits.push(get_bit(data, i));
    }

    for i in 0..insert_bit_len {
        out_bits.push(get_bit(insert_bits, i));
    }

    for i in end_remove..total_bits {
        out_bits.push(get_bit(data, i));
    }
    log::debug!("Splice: start_bit={}, remove_bits={}, insert_bit_len={}", 
             start_bit, remove_bits, insert_bit_len);
    bits_to_bytes(&out_bits)
}

/// Пишет `len` бит `val` в `buf` начиная с `bit_offset`.
fn write_bits(buf: &mut [u8], bit_offset: usize, len: usize, val: u32) {
    for i in 0..len {
        let bit = (val >> (len - 1 - i)) & 1;
        let pos = bit_offset + i;
        let byte = pos / 8;
        if byte >= buf.len() {
            break;
        }
        let shift = 7 - (pos % 8);
        buf[byte] = (buf[byte] & !(1 << shift)) | ((bit as u8) << shift);
    }
}

/// Читает HEVC RBSP побитово (с удалением emulation prevention bytes 0x000003)
pub struct RbspReader<'a> {
    bit_pos: usize,
    cleaned: Vec<u8>,
    _phantom: std::marker::PhantomData<&'a [u8]>,
}

impl<'a> RbspReader<'a> {
    pub fn new(nal_data: &'a [u8]) -> Self {

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

                i += 3;

            } else {

                cleaned.push(nal_data[i]);

                i += 1;

            }

        }

        Self { bit_pos: 0, cleaned, _phantom: std::marker::PhantomData }

    }

    pub fn read_bit(&mut self) -> u32 {
        let byte = self.bit_pos / 8;
        let bit = 7 - (self.bit_pos % 8);
        self.bit_pos += 1;
        if byte < self.cleaned.len() {
            ((self.cleaned[byte] >> bit) & 1) as u32
        } else {
            0
        }
    }

    pub fn read_bits(&mut self, n: usize) -> u32 {
        let mut val = 0u32;
        for _ in 0..n {
            val = (val << 1) | self.read_bit();
        }
        val
    }

    pub fn read_ue(&mut self) -> u32 {
        let mut leading = 0usize;
        while self.read_bit() == 0 {
            leading += 1;
            if leading > 31 {
                return 0;
            }
        }
        if leading == 0 {
            return 0;
        }
        let suffix = self.read_bits(leading);
        ((1u32 << leading) - 1) + suffix
    }

    pub fn read_se(&mut self) -> i32 {
        let ue = self.read_ue();
        let abs = ((ue + 1) / 2) as i32;
        if ue & 1 == 1 { abs } else { -abs }
    }

    pub fn current_bit_pos(&self) -> usize {
        self.bit_pos
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
    pub pps_id: u8,
}

impl ShortTermRps {
    pub fn serialize_inline_compact(&self, max_bits: usize) -> Option<(Vec<u8>, usize)> {
        let mut neg = self.negative.clone();
        let mut pos = self.positive.clone();

        loop {
            let mut w = BitWriter::new();

            // inter_ref_pic_set_prediction_flag = 0
            w.write_bit(false);
            w.write_ue(neg.len() as u32);
            w.write_ue(pos.len() as u32);

            for pic in &neg {
                let d = pic.delta_poc.abs() as u32;
                w.write_ue(d.saturating_sub(1));
                w.write_bit(pic.used_by_curr);
            }

            for pic in &pos {
                let d = pic.delta_poc.abs() as u32;
                w.write_ue(d.saturating_sub(1));
                w.write_bit(pic.used_by_curr);
            }

            let bit_len = w.bit_len();
            if bit_len <= max_bits {
                let bytes = w.into_bytes();
                return Some((bytes, bit_len));
            }

            if neg.is_empty() && pos.is_empty() {
                return None;
            }

            let drop_neg = match (neg.last(), pos.last()) {
                (Some(n), Some(p)) => n.delta_poc.abs() >= p.delta_poc.abs(),
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => false,
            };

            if drop_neg {
                neg.pop();
            } else {
                pos.pop();
            }
        }
    }
}

impl SkipFrameTemplate {
    pub fn make_frame(&self, poc_lsb: u32, state: &HevcState) -> Option<Vec<u8>> {
        let poc = poc_lsb & (self.max_poc_lsb - 1);

        // derive_abs_poc корректно обрабатывает MSB wrap-around — убираем хак +1
        let curr_abs_poc = state.derive_abs_poc(poc_lsb);

        let rps = state.current_concealment_rps(curr_abs_poc)?;

        // max_bits для inline RPS = оригинал минус 1 (sps_flag добавим сами)
        let (rps_bytes, rps_bit_len) =
            rps.serialize_inline_compact(self.rps_original_bit_len.saturating_sub(1))?;

        // Собираем полное поле RPS в slice header:
        // short_term_ref_pic_set_sps_flag = 0   (1 бит)
        // short_term_ref_pic_set()              (rps_bit_len бит)
        let mut full = BitWriter::new();
        full.write_bit(false); // sps_flag = 0 → inline
        for i in 0..rps_bit_len {
            full.write_bit(get_bit(&rps_bytes, i) != 0);
        }
        let full_bytes = full.into_bytes();
        let full_bit_len = rps_bit_len + 1;

        let mut payload = self.data.clone();

        write_bits(&mut payload, self.poc_lsb_bit_offset, self.poc_lsb_len as usize, poc);

        payload = splice_bits(
            &payload,
            self.rps_bit_offset,
            self.rps_original_bit_len,
            &full_bytes,
            full_bit_len,
        );

        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        frame.extend_from_slice(&payload);
        Some(frame)
    }

    pub(crate) fn parse_sps_poc_bits(nalu: &[u8]) -> Option<u8> {
        if nalu.len() < 3 {return None;}
        let mut r = RbspReader::new(&nalu[2..]);

        r.read_bits(4); 
        let max_sub_layers = r.read_bits(3); 
        r.read_bit();
        Self::skip_profile_tier_level(&mut r, true, max_sub_layers);

        r.read_ue(); // sps_seq_parameter_set_id 
        let chroma_format = r.read_ue(); 
        if chroma_format == 3 { 
            r.read_bit();
        } 
        r.read_ue(); 
        r.read_ue();
        
        let conformance_window = r.read_bit(); 
        if conformance_window == 1 { 
            r.read_ue(); 
            r.read_ue(); 
            r.read_ue(); 
            r.read_ue(); 
        } 
        r.read_ue();
        r.read_ue();

        let log2_max = r.read_ue(); 
        Some(log2_max as u8)
    }

    fn skip_profile_tier_level(r: &mut RbspReader, profile_present: bool, max_sub_layers: u32) { 
        if profile_present { 
            r.read_bits(2); // general_profile_space 
            r.read_bit(); // general_tier_flag 
            r.read_bits(5); // general_profile_idc 
            r.read_bits(32); // general_profile_compatibility_flag[32] 
            r.read_bit(); // general_progressive_source_flag 
            r.read_bit(); // general_interlaced_source_flag 
            r.read_bit(); // general_non_packed_constraint_flag 
            r.read_bit(); // general_frame_only_constraint_flag 
            r.read_bits(32); // general_reserved_zero_44bits (часть) 
            r.read_bits(12); r.read_bits(8); // general_level_idc 
        } 
        let mut sub_layer_profile = vec![false; max_sub_layers as usize]; 
        let mut sub_layer_level = vec![false; max_sub_layers as usize]; 
        for i in 0..max_sub_layers as usize { 
            sub_layer_profile[i] = r.read_bit() == 1; 
            sub_layer_level[i] = r.read_bit() == 1; 
        } 
        for i in 0..max_sub_layers as usize { 
            if sub_layer_profile[i] { 
                r.read_bits(2); 
                r.read_bit(); 
                r.read_bits(5); 
                r.read_bits(32); 
                r.read_bits(4); 
                r.read_bits(32); 
                r.read_bits(12); 
                r.read_bits(8); 
            } 
            if sub_layer_level[i] { 
                r.read_bits(8); 
            } 
        } 
    }
}

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
    pub pps_id: u8,
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

        let pps_id = r.read_ue() as u8;
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

        // poc_lsb отсутствует ТОЛЬКО в IDR кадрах (19 и 20)
        let poc_lsb = if nal_type != 19 && nal_type != 20 {
            r.read_bits(sps.log2_max_poc_lsb as usize)
        } else {
            0
        };

        // После чтения poc_lsb:
        let rps_start_pos = r.current_bit_pos();
        let rps_sps_flag = r.read_bit();

        if rps_sps_flag == 1 {
            // Если ссылка на таблицу в SPS, это просто индекс (UE)
            r.read_ue(); 
        } else {
            // Inline RPS. Читаем строго по спецификации HEVC
            let num_neg = r.read_ue();
            let num_pos = r.read_ue();
            log::info!("Splice: num neg: {}, num_pos: {}", num_neg, num_pos);
            if num_neg > 15 || num_pos > 15 { return None; }
            // ОГРАНИЧЕНИЕ: Если num_neg или num_pos слишком большие (> 16), 
            // значит мы парсим мусор. Прерываемся.
            if num_neg > 16 || num_pos > 16 {
                return None; 
            }

            for _ in 0..num_neg {
                r.read_ue();  // delta_poc_s0_minus1
                r.read_bit(); // used_by_curr_pic_s0_flag
            }
            for _ in 0..num_pos {
                r.read_ue();  // delta_poc_s1_minus1
                r.read_bit(); // used_by_curr_pic_s1_flag
            }
        }
        let rps_end_pos = r.current_bit_pos();
        let rps_original_bit_len = rps_end_pos - rps_start_pos;

        log::trace!(
            "[Concealment] SliceParse nal_type={} first_slice={} poc_lsb={} poc_off={} poc_len={} rps_off={} rps_len={}",
            nal_type,
            first_slice,
            poc_lsb,
            poc_bit_offset,
            sps.log2_max_poc_lsb,
            rps_start_pos + 16,
            rps_original_bit_len,
        );
        Some(Self {
            poc_lsb,
            poc_lsb_bit_offset: poc_bit_offset + 16,
            poc_lsb_len: sps.log2_max_poc_lsb,
            rps_bit_offset: rps_start_pos + 16,
            rps_original_bit_len,
            pps_id
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct SpsFields {
    pub raw:                                    Vec<u8>,
    pub log2_max_poc_lsb_minus4:                u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2:   u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter:    u8,
    pub max_transform_hierarchy_depth_intra:    u8,
    pub sps_max_dec_pic_buffering_minus1:       u8,
    pub amp_enabled_flag:                       bool,
    pub sample_adaptive_offset_enabled_flag:    bool,
    pub pcm_enabled_flag:                       bool,
    pub num_short_term_ref_pic_sets:            u8,
    pub long_term_ref_pics_present_flag:        bool,
    pub sps_temporal_mvp_enabled_flag:          bool,
    pub strong_intra_smoothing_enabled_flag:    bool,
    pub scaling_list_enabled_flag:              bool,
}
 
impl SpsFields {
    /// Парсит SPS NALU (включая NAL header, без Annex B start code).
    ///
    /// Байты из логов: head=[42, 01, 01, 01, 60, 00...] для nal_type=33.
    pub fn parse(nalu: &[u8]) -> Option<Self> {
        if nalu.len() < 4 { return None; }
        let raw = nalu.to_vec();
        // Первые 2 байта — NAL unit header, пропускаем
        let mut r = RbspReader::new(&nalu[2..]);
 
        // sps_video_parameter_set_id (4 бита)
        r.read_bits(4);
        // sps_max_sub_layers_minus1 (3 бита)
        let max_sub_layers = r.read_bits(3);
        // sps_temporal_id_nesting_flag (1 бит)
        r.read_bit();
 
        // profile_tier_level(profile_present_flag=1, sps_max_sub_layers_minus1)
        SkipFrameTemplate::skip_profile_tier_level(&mut r, true, max_sub_layers);
 
        // sps_seq_parameter_set_id
        r.read_ue();
 
        // chroma_format_idc
        let chroma_format_idc = r.read_ue();
        if chroma_format_idc == 3 {
            r.read_bit(); // separate_colour_plane_flag
        }
 
        // pic_width_in_luma_samples
        r.read_ue();
        // pic_height_in_luma_samples
        r.read_ue();
 
        // conformance_window_flag
        if r.read_bit() == 1 {
            r.read_ue(); // conf_win_left_offset
            r.read_ue(); // conf_win_right_offset
            r.read_ue(); // conf_win_top_offset
            r.read_ue(); // conf_win_bottom_offset
        }
 
        // bit_depth_luma_minus8
        r.read_ue();
        // bit_depth_chroma_minus8
        r.read_ue();
 
        // log2_max_pic_order_cnt_lsb_minus4
        let log2_max_poc_lsb_minus4 = r.read_ue() as u8;
 
        // sps_sub_layer_ordering_info_present_flag
        let sub_layer_ordering = r.read_bit();
        let num_iter = if sub_layer_ordering == 1 { max_sub_layers + 1 } else { 1 };
        let mut sps_max_dec_pic_buffering_minus1 = 0u8;
        for i in 0..num_iter {
            let buf = r.read_ue() as u8;
            if i == num_iter - 1 {
                sps_max_dec_pic_buffering_minus1 = buf;
            }
            r.read_ue(); // sps_max_num_reorder_pics
            r.read_ue(); // sps_max_latency_increase_plus1
        }
 
        // log2_min_luma_coding_block_size_minus3
        let log2_min_luma_coding_block_size_minus3 = r.read_ue() as u8;
        // log2_diff_max_min_luma_coding_block_size
        let log2_diff_max_min_luma_coding_block_size = r.read_ue() as u8;
        // log2_min_luma_transform_block_size_minus2
        let log2_min_transform_block_size_minus2 = r.read_ue() as u8;
        // log2_diff_max_min_luma_transform_block_size
        let log2_diff_max_min_transform_block_size = r.read_ue() as u8;
        // max_transform_hierarchy_depth_inter
        let max_transform_hierarchy_depth_inter = r.read_ue() as u8;
        // max_transform_hierarchy_depth_intra
        let max_transform_hierarchy_depth_intra = r.read_ue() as u8;
 
        // scaling_list_enabled_flag
        let scaling_list_enabled_flag = r.read_bit() == 1;
        if scaling_list_enabled_flag {
            if r.read_bit() == 1 { // sps_scaling_list_data_present_flag
                skip_scaling_list_data(&mut r);
            }
        }
 
        // amp_enabled_flag
        let amp_enabled_flag = r.read_bit() == 1;
        // sample_adaptive_offset_enabled_flag
        let sample_adaptive_offset_enabled_flag = r.read_bit() == 1;
 
        // pcm_enabled_flag
        let pcm_enabled_flag = r.read_bit() == 1;
        if pcm_enabled_flag {
            r.read_bits(4); // pcm_sample_bit_depth_luma_minus1
            r.read_bits(4); // pcm_sample_bit_depth_chroma_minus1
            r.read_ue();    // log2_min_pcm_luma_coding_block_size_minus3
            r.read_ue();    // log2_diff_max_min_pcm_luma_coding_block_size
            r.read_bit();   // pcm_loop_filter_disabled_flag
        }
 
        // num_short_term_ref_pic_sets
        let num_short_term_ref_pic_sets = r.read_ue() as u8;
        for i in 0..num_short_term_ref_pic_sets as u32 {
            skip_short_term_ref_pic_set(&mut r, i);
        }
 
        // long_term_ref_pics_present_flag
        let long_term_ref_pics_present_flag = r.read_bit() == 1;
        if long_term_ref_pics_present_flag {
            let num_lt_sps = r.read_ue();
            let poc_bits = log2_max_poc_lsb_minus4 as usize + 4;
            for _ in 0..num_lt_sps {
                r.read_bits(poc_bits); // lt_ref_pic_poc_lsb_sps
                r.read_bit();          // used_by_curr_pic_lt_sps_flag
            }
        }
 
        // sps_temporal_mvp_enabled_flag
        let sps_temporal_mvp_enabled_flag = r.read_bit() == 1;
        // strong_intra_smoothing_enabled_flag
        let strong_intra_smoothing_enabled_flag = r.read_bit() == 1;
 
        Some(SpsFields {
            raw,
            log2_max_poc_lsb_minus4,
            log2_min_luma_coding_block_size_minus3,
            log2_diff_max_min_luma_coding_block_size,
            log2_min_transform_block_size_minus2,
            log2_diff_max_min_transform_block_size,
            max_transform_hierarchy_depth_inter,
            max_transform_hierarchy_depth_intra,
            sps_max_dec_pic_buffering_minus1,
            amp_enabled_flag,
            sample_adaptive_offset_enabled_flag,
            pcm_enabled_flag,
            num_short_term_ref_pic_sets,
            long_term_ref_pics_present_flag,
            sps_temporal_mvp_enabled_flag,
            strong_intra_smoothing_enabled_flag,
            scaling_list_enabled_flag,
        })
    }
}
 
/// Все поля PPS нужные для VAPictureParameterBufferHEVC и генерации slice header.
///
/// Парсится из NAL nal_type=34.
#[derive(Debug, Clone, Default)]
pub struct PpsFields {
    pub raw:                                    Vec<u8>,
    pub pps_pic_parameter_set_id:                 u8,
    pub pps_seq_parameter_set_id:                 u8,
    pub dependent_slice_segments_enabled_flag:    bool,
    pub output_flag_present_flag:                 bool,
    pub num_extra_slice_header_bits:              u8,
    pub sign_data_hiding_enabled_flag:            bool,
    pub cabac_init_present_flag:                  bool,
    pub num_ref_idx_l0_default_active_minus1:     u8,
    pub num_ref_idx_l1_default_active_minus1:     u8,
    pub init_qp_minus26:                          i8,
    pub constrained_intra_pred_flag:              bool,
    pub transform_skip_enabled_flag:              bool,
    pub cu_qp_delta_enabled_flag:                 bool,
    pub diff_cu_qp_delta_depth:                   u8,
    pub pps_cb_qp_offset:                         i8,
    pub pps_cr_qp_offset:                         i8,
    pub pps_slice_chroma_qp_offsets_present_flag: bool,
    pub weighted_pred_flag:                       bool,
    pub weighted_bipred_flag:                     bool,
    pub transquant_bypass_enabled_flag:           bool,
    pub tiles_enabled_flag:                       bool,
    pub entropy_coding_sync_enabled_flag:         bool,
    pub loop_filter_across_slices_enabled_flag:   bool,
    pub deblocking_filter_override_enabled_flag:  bool,
    pub pps_disable_deblocking_filter_flag:       bool,
    pub pps_beta_offset_div2:                     i8,
    pub pps_tc_offset_div2:                       i8,
    pub pps_scaling_list_data_present_flag:       bool,
    pub lists_modification_present_flag:          bool,
    pub log2_parallel_merge_level_minus2:         u8,
    pub slice_segment_header_extension_present_flag: bool,
}
 
impl PpsFields {
    /// Парсит PPS NALU (включая NAL header, без start code) — nal_type=34.
    ///
    /// Байты из логов: head=[44, 01, E0, 73, C0, 89] → pps_id=5.
    pub fn parse(nalu: &[u8]) -> Option<Self> {
        if nalu.len() < 3 { return None; }
        let raw = nalu.to_vec();
        let mut r = RbspReader::new(&nalu[2..]); // пропускаем NAL header
 
        let pps_pic_parameter_set_id = r.read_ue() as u8;
        let pps_seq_parameter_set_id = r.read_ue() as u8;
        let dependent_slice_segments_enabled_flag = r.read_bit() == 1;
        let output_flag_present_flag              = r.read_bit() == 1;
        let num_extra_slice_header_bits           = r.read_bits(3) as u8;
        let sign_data_hiding_enabled_flag         = r.read_bit() == 1;
        let cabac_init_present_flag               = r.read_bit() == 1;
        let num_ref_idx_l0_default_active_minus1  = r.read_ue() as u8;
        let num_ref_idx_l1_default_active_minus1  = r.read_ue() as u8;
        let init_qp_minus26                       = r.read_se() as i8;
        let constrained_intra_pred_flag           = r.read_bit() == 1;
        let transform_skip_enabled_flag           = r.read_bit() == 1;
 
        let cu_qp_delta_enabled_flag = r.read_bit() == 1;
        let diff_cu_qp_delta_depth   = if cu_qp_delta_enabled_flag {
            r.read_ue() as u8
        } else {
            0
        };
 
        let pps_cb_qp_offset = r.read_se() as i8;
        let pps_cr_qp_offset = r.read_se() as i8;
        let pps_slice_chroma_qp_offsets_present_flag = r.read_bit() == 1;
        let weighted_pred_flag    = r.read_bit() == 1;
        let weighted_bipred_flag  = r.read_bit() == 1;
        let transquant_bypass_enabled_flag = r.read_bit() == 1;
 
        let tiles_enabled_flag              = r.read_bit() == 1;
        let entropy_coding_sync_enabled_flag = r.read_bit() == 1;
 
        if tiles_enabled_flag {
            let num_col = r.read_ue();
            let num_row = r.read_ue();
            if r.read_bit() == 0 { // uniform_spacing_flag = 0
                for _ in 0..num_col { r.read_ue(); } // column_width_minus1
                for _ in 0..num_row { r.read_ue(); } // row_height_minus1
            }
            r.read_bit(); // loop_filter_across_tiles_enabled_flag
        }
 
        let loop_filter_across_slices_enabled_flag = r.read_bit() == 1;
 
        let deblocking_filter_control_present = r.read_bit() == 1;
        let (
            deblocking_filter_override_enabled_flag,
            pps_disable_deblocking_filter_flag,
            pps_beta_offset_div2,
            pps_tc_offset_div2,
        ) = if deblocking_filter_control_present {
            let ovr = r.read_bit() == 1;
            let dis = r.read_bit() == 1;
            let (beta, tc) = if !dis {
                (r.read_se() as i8, r.read_se() as i8)
            } else {
                (0, 0)
            };
            (ovr, dis, beta, tc)
        } else {
            (false, false, 0, 0)
        };
 
        let pps_scaling_list_data_present_flag = r.read_bit() == 1;
        if pps_scaling_list_data_present_flag {
            skip_scaling_list_data(&mut r);
        }
 
        let lists_modification_present_flag          = r.read_bit() == 1;
        let log2_parallel_merge_level_minus2          = r.read_ue() as u8;
        let slice_segment_header_extension_present_flag = r.read_bit() == 1;
 
        Some(PpsFields {
            raw,
            pps_pic_parameter_set_id,
            pps_seq_parameter_set_id,
            dependent_slice_segments_enabled_flag,
            output_flag_present_flag,
            num_extra_slice_header_bits,
            sign_data_hiding_enabled_flag,
            cabac_init_present_flag,
            num_ref_idx_l0_default_active_minus1,
            num_ref_idx_l1_default_active_minus1,
            init_qp_minus26,
            constrained_intra_pred_flag,
            transform_skip_enabled_flag,
            cu_qp_delta_enabled_flag,
            diff_cu_qp_delta_depth,
            pps_cb_qp_offset,
            pps_cr_qp_offset,
            pps_slice_chroma_qp_offsets_present_flag,
            weighted_pred_flag,
            weighted_bipred_flag,
            transquant_bypass_enabled_flag,
            tiles_enabled_flag,
            entropy_coding_sync_enabled_flag,
            loop_filter_across_slices_enabled_flag,
            deblocking_filter_override_enabled_flag,
            pps_disable_deblocking_filter_flag,
            pps_beta_offset_div2,
            pps_tc_offset_div2,
            pps_scaling_list_data_present_flag,
            lists_modification_present_flag,
            log2_parallel_merge_level_minus2,
            slice_segment_header_extension_present_flag,
        })
    }
}
 
// ── Вспомогательные функции для парсинга (добавить как свободные функции) ───
 
fn skip_scaling_list_data(r: &mut RbspReader) {
    for size_id in 0..4u32 {
        let num_matrix = if size_id == 3 { 2u32 } else { 6 };
        for _ in 0..num_matrix {
            if r.read_bit() == 0 { // scaling_list_pred_mode_flag = 0
                r.read_ue(); // scaling_list_pred_matrix_id_delta
            } else {
                let num_coef = if size_id == 0 { 16u32 } else { 64 };
                if size_id > 1 {
                    r.read_se(); // scaling_list_dc_coef_minus8
                }
                for _ in 0..num_coef {
                    r.read_se(); // scaling_list_delta_coef
                }
            }
        }
    }
}
 
fn skip_short_term_ref_pic_set(r: &mut RbspReader, st_rps_idx: u32) {
    // inter_ref_pic_set_prediction_flag (только если idx > 0)
    let inter_flag = if st_rps_idx > 0 { r.read_bit() } else { 0 };
    if inter_flag == 1 {
        r.read_ue(); // delta_idx_minus1
        r.read_bit(); // delta_rps_sign
        r.read_ue(); // abs_delta_rps_minus1
        // Читаем num_delta_pocs флагов.
        // Спецификация требует знать num_delta_pocs предыдущего RPS —
        // для uprosheniya читаем фиксированные 17 пар (max refs + 1).
        for _ in 0..17u32 {
            let used = r.read_bit();
            if used == 0 {
                r.read_bit(); // use_delta_flag
            }
        }
    } else {
        let num_neg = r.read_ue();
        let num_pos = r.read_ue();
        for _ in 0..num_neg {
            r.read_ue(); // delta_poc_s0_minus1
            r.read_bit(); // used_by_curr_pic_s0_flag
        }
        for _ in 0..num_pos {
            r.read_ue(); // delta_poc_s1_minus1
            r.read_bit(); // used_by_curr_pic_s1_flag
        }
    }
}
 
// ─────────────────────────────────────────────────────────────────────────────
// build_concealment_pic_params — собирает ConcealmentPicParams из SPS+PPS
// ─────────────────────────────────────────────────────────────────────────────
 
use crate::backend::vaapi_concealment::ConcealmentPicParams;
 
/// Строит ConcealmentPicParams с правильными pic_fields и slice_parsing_fields
/// для AMD radeonsi (Mesa). Mesa строго валидирует соответствие этих полей
/// реальному bitstream в vaEndPicture.
pub fn build_concealment_pic_params(
    width:  u16,
    height: u16,
    sps:    &SpsFields,
    pps:    &PpsFields,
) -> ConcealmentPicParams {
    // pic_fields bits (va/va_dec_hevc.h, VAPictureLongFields):
    let pic_fields: u32 =
          (pps.tiles_enabled_flag as u32)                          <<  0
        | (pps.entropy_coding_sync_enabled_flag as u32)            <<  1
        | (pps.sign_data_hiding_enabled_flag as u32)               <<  2
        | (sps.scaling_list_enabled_flag as u32)                   <<  3
        | (pps.transform_skip_enabled_flag as u32)                 <<  4
        | (sps.amp_enabled_flag as u32)                            <<  5
        | (sps.strong_intra_smoothing_enabled_flag as u32)         <<  6
        | (sps.pcm_enabled_flag as u32)                            <<  7
        // bit 8: pcm_loop_filter_disabled_flag — нет в нашем парсинге, оставляем 0
        | (pps.weighted_pred_flag as u32)                          <<  9
        | (pps.weighted_bipred_flag as u32)                        << 10
        // bit 11: loop_filter_across_tiles — 0 (tiles_enabled=0 в большинстве потоков)
        | (pps.loop_filter_across_slices_enabled_flag as u32)      << 12
        | (pps.output_flag_present_flag as u32)                    << 13
        | ((pps.num_extra_slice_header_bits > 0) as u32)           << 14
        | (pps.pps_slice_chroma_qp_offsets_present_flag as u32)    << 15
        | (pps.deblocking_filter_override_enabled_flag as u32)     << 16
        | (pps.pps_disable_deblocking_filter_flag as u32)          << 17
        | (pps.lists_modification_present_flag as u32)             << 18
        | (pps.slice_segment_header_extension_present_flag as u32) << 19;
 
    // slice_parsing_fields bits (va/va_dec_hevc.h):
    let slice_parsing_fields: u32 =
          (pps.lists_modification_present_flag as u32)             <<  0
        | (sps.long_term_ref_pics_present_flag as u32)             <<  1
        | (sps.sps_temporal_mvp_enabled_flag as u32)               <<  2
        | (pps.cabac_init_present_flag as u32)                     <<  3
        | (pps.output_flag_present_flag as u32)                    <<  4
        | (pps.dependent_slice_segments_enabled_flag as u32)       <<  5
        | (pps.pps_slice_chroma_qp_offsets_present_flag as u32)    <<  6
        | (sps.sample_adaptive_offset_enabled_flag as u32)         <<  7
        | (pps.deblocking_filter_override_enabled_flag as u32)     << 10
        | (pps.pps_disable_deblocking_filter_flag as u32)          << 11
        | (pps.slice_segment_header_extension_present_flag as u32) << 12;
        // Bits 13,14,15: RapPicFlag, IdrPicFlag, IntraPicFlag — 0 для TRAIL_R P-frame
 
    log::info!(
        "[Concealment] build_concealment_pic_params: \
         pps_id={} pic_fields={:#010x} slice_parsing_fields={:#010x} \
         amp={} sao={} temporal_mvp={} deblock_override={} disable_deblock={} \
         init_qp={} extra_bits={}",
        pps.pps_pic_parameter_set_id,
        pic_fields,
        slice_parsing_fields,
        sps.amp_enabled_flag,
        sps.sample_adaptive_offset_enabled_flag,
        sps.sps_temporal_mvp_enabled_flag,
        pps.deblocking_filter_override_enabled_flag,
        pps.pps_disable_deblocking_filter_flag,
        pps.init_qp_minus26,
        pps.num_extra_slice_header_bits,
    );
 
    ConcealmentPicParams {
        width,
        height,
        log2_max_poc_lsb_minus4: sps.log2_max_poc_lsb_minus4,
        log2_min_luma_coding_block_size_minus3: sps.log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size: sps.log2_diff_max_min_luma_coding_block_size,
        log2_min_transform_block_size_minus2: sps.log2_min_transform_block_size_minus2,
        log2_diff_max_min_transform_block_size: sps.log2_diff_max_min_transform_block_size,
        max_transform_hierarchy_depth_intra: sps.max_transform_hierarchy_depth_intra,
        max_transform_hierarchy_depth_inter: sps.max_transform_hierarchy_depth_inter,
        init_qp_minus26: pps.init_qp_minus26,
        num_short_term_ref_pic_sets: sps.num_short_term_ref_pic_sets,
        pps_id: pps.pps_pic_parameter_set_id,
        num_ref_idx_l0_default_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        pic_fields,
        slice_parsing_fields,
        // Новые поля для генерации slice header:
        deblocking_filter_override_enabled: pps.deblocking_filter_override_enabled_flag,
        pps_disable_deblocking_filter:      pps.pps_disable_deblocking_filter_flag,
        sps_temporal_mvp_enabled:           sps.sps_temporal_mvp_enabled_flag,
        sao_enabled:                        sps.sample_adaptive_offset_enabled_flag,
        cabac_init_present:                 pps.cabac_init_present_flag,
        slice_chroma_qp_offsets_present:    pps.pps_slice_chroma_qp_offsets_present_flag,
        pps_cb_qp_offset:                   pps.pps_cb_qp_offset,
        pps_cr_qp_offset:                   pps.pps_cr_qp_offset,
        pps_beta_offset_div2:               pps.pps_beta_offset_div2,
        pps_tc_offset_div2:                 pps.pps_tc_offset_div2,
        log2_parallel_merge_level_minus2:   pps.log2_parallel_merge_level_minus2,
        num_extra_slice_header_bits:        pps.num_extra_slice_header_bits,
        diff_cu_qp_delta_depth:             pps.diff_cu_qp_delta_depth,
    }
}
 