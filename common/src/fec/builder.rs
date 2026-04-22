use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::{
    fec::decode::FecDecoder,
    DatagramChunk,
    FrameTrace,
    VideoPacket,
    VideoSlice,
};

use super::*;

// ── GroupState ────────────────────────────────────────────────────────────────
//
// Стейт-машина для отдельной FEC-группы.
//
// Переходы:
//
//   Collecting ──(потери > M)──► Stalled ──(NACK отправлен)──► Requested
//        │                           │                              │
//        │                    (получили K шардов)           (получили K шардов)
//        │                           │                              │
//        └──────(получили K шардов)──┴──────────────────────────────┴──► Ready
//
//   Любое состояние ──(кадр устарел)──► Abandoned
//
// `Stalled` означает: потерь больше, чем паритетных шардов — FEC не поможет,
// нужен NACK.  `Requested` означает: NACK уже отправлен, группа «спит» до
// истечения `NACK_SUPPRESS_US` прежде чем снова стать `Stalled`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GroupState {
    /// Дырки есть, но потерь ≤ M — FEC может справиться, ждём.
    Collecting,
    /// Потерь > M.  FEC не спасёт.  `FrameAssembler` должен запросить NACK.
    /// Поле `since_us` — момент перехода в это состояние; используется для
    /// контроля `MIN_NACK_AGE_US`.
    Stalled { since_us: u64 },
    /// NACK отправлен.  Ждём `NACK_SUPPRESS_US` прежде чем повторить.
    /// Поле `sent_at_us` — момент отправки последнего NACK.
    Requested { sent_at_us: u64 },
    /// Группа полностью собрана (FEC или напрямую).
    Ready,
    /// Кадр истёк; группа отброшена.
    Abandoned,
    Decoding,
}

impl GroupState {
    /// Возвращает `true`, если группа требует NACK прямо сейчас.
    pub(crate) fn needs_nack(&self, now_us: u64) -> bool {
        match self {
            // Ждём MIN_NACK_AGE_US после первого обнаружения потерь.
            GroupState::Stalled { since_us } => {
                now_us.saturating_sub(*since_us) >= MIN_NACK_AGE_US
            }
            // Повторный NACK допустим только после NACK_SUPPRESS_US.
            GroupState::Requested { sent_at_us } => {
                now_us.saturating_sub(*sent_at_us) >= NACK_SUPPRESS_US
            }
            _ => false,
        }
    }
}

// ── GroupRecovery ─────────────────────────────────────────────────────────────

pub(crate) enum GroupRecovery {
    Direct,
    Fec { missing_data_shards: u8 },
}

// ── DecodeTask / DecodeResult ─────────────────────────────────────────────────
//
// Задача FEC-декодирования, которую можно выполнить в пуле потоков (rayon),
// не блокируя поток приёма UDP.

/// Всё, что нужно воркеру для реконструкции одной группы.
pub(crate) struct DecodeTask {
    pub(crate) frame_id:   u64,
    pub(crate) slice_idx:  u8,
    pub(crate) group_idx:  u8,
    pub(crate) k:          u8,
    pub(crate) m:          u8,
    /// Snapshot шардов на момент постановки задачи.
    pub(crate) shards:     Vec<Option<Bytes>>,
    pub(crate) payload_lens: Vec<u16>,
}

/// Результат, который воркер кладёт обратно в очередь.
pub(crate) struct DecodeResult {
    pub(crate) frame_id:  u64,
    pub(crate) slice_idx: u8,
    /// `None` — декодирование не удалось.
    pub(crate) data:      Option<Vec<u8>>,
}

// ── ComputePool ───────────────────────────────────────────────────────────────
//
// Тонкая обёртка над `rayon::spawn` + очередью результатов.
// `RS_CACHE` в `decode.rs` помечен `thread_local!` — каждый поток rayon
// создаёт свой экземпляр `ReedSolomon` и переиспользует его без конкуренции.

pub struct ComputePool {
    /// Готовые результаты декодирования, которые ещё не забрал `FrameAssembler`.
    results: Arc<Mutex<Vec<DecodeResult>>>,
}

impl ComputePool {
    pub fn new() -> Self {
        Self {
            results: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Поставить задачу FEC-декодирования в пул.
    pub(crate) fn submit(&self, task: DecodeTask) {
        let sink = Arc::clone(&self.results);
        rayon::spawn(move || {
            let data = FecDecoder::decode(task.k, task.m, &task.shards, &task.payload_lens);
            let result = DecodeResult {
                frame_id:  task.frame_id,
                slice_idx: task.slice_idx,
                data,
            };
            if let Ok(mut guard) = sink.lock() {
                guard.push(result);
            }
        });
    }

    /// Забрать все готовые результаты (non-blocking).
    pub(crate) fn drain_results(&self) -> Vec<DecodeResult> {
        self.results
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }
}

// ── FrameBuilder ─────────────────────────────────────────────────────────────

pub(crate) struct FrameBuilder {
    pub(crate) slices: HashMap<u8, SliceBuilder>,
    decoded_slices: Vec<Option<VideoSlice>>,
    pub(crate) total_slices: u8,
    pub(crate) first_us: u64,
    next_emit_idx: u8,
    /// Packets that have been decoded and are waiting for the HOL flush loop
    /// to release them in order.  Populated incrementally as slices complete;
    /// drained all at once by [`take_packets`] when the frame is complete.
    ready_packets: Vec<VideoPacket>,
}

impl FrameBuilder {
    pub(crate) fn new(total_slices: u8, _is_key: bool) -> Self {
        let mut decoded_slices = Vec::with_capacity(total_slices as usize);
        decoded_slices.resize_with(total_slices as usize, || None);

        Self {
            slices: HashMap::new(),
            decoded_slices,
            total_slices,
            first_us: FrameTrace::now_us(),
            next_emit_idx: 0,
            ready_packets: Vec::new(),
        }
    }

    /// Returns `true` once every slice has been decoded and staged in
    /// `ready_packets`.
    pub(crate) fn is_complete(&self) -> bool {
        self.next_emit_idx == self.total_slices
    }

    pub(crate) fn count_wasted(&self) -> (u64, u64, u64) {
        let mut failed_groups = 0u64;
        let mut wasted_payload_bytes = 0u64;
        let mut lost_chunks = 0u64;

        for sb in self.slices.values() {
            for gb in sb.groups.values() {
                if gb.state != GroupState::Ready {
                    failed_groups += 1;
                    let total_expected = (gb.k + gb.m) as u64;
                    lost_chunks += total_expected.saturating_sub(gb.received as u64);
                }
                for i in 0..(gb.k as usize) {
                    if gb.shards[i].is_some() {
                        wasted_payload_bytes += gb.payload_lens[i] as u64;
                    }
                }
            }
        }

        (failed_groups, wasted_payload_bytes, lost_chunks)
    }

    /// Insert a chunk into this builder.
    ///
    /// Если группа стала готова к FEC-восстановлению И для неё нет
    /// синхронного fast-path (все data-шарды на месте), возвращает
    /// `DecodeTask` для асинхронной обработки в `ComputePool`.
    /// Если fast-path сработал — декодирует синхронно и возвращает `None`.
    
    pub(crate) fn insert_chunk(
        &mut self,
        chunk: &DatagramChunk,
    ) -> (Vec<GroupRecovery>, Option<DecodeTask>, bool, Option<(u8, u8)>) {
        let slice_idx = chunk.slice_idx as usize;
        if slice_idx >= self.decoded_slices.len() {
            return (Vec::new(), None, false, None);
        }

        let mut recoveries = Vec::new();
        let mut maybe_task: Option<DecodeTask> = None;

        let (group_ready, needs_fec, is_nack_recovery, newly_stalled) = {
            let sb = self
                .slices
                .entry(chunk.slice_idx)
                .or_insert_with(|| SliceBuilder::new(chunk.total_groups));

            let (maybe_recovery, is_nack_recovery, stalled_flag) = sb.insert(chunk);

            if let Some(recovery) = maybe_recovery {
                recoveries.push(recovery);
            }

            let group_ready = sb.is_ready();

            let needs_fec = if group_ready {
                let gb = sb.groups.get_mut(&chunk.group_idx).unwrap();
                if gb.state != GroupState::Decoding && gb.state != GroupState::Ready && gb.data_received_count() < gb.k {
                    gb.state = GroupState::Decoding;
                    true
                } else {
                    false
                }
            } else { false };

            let newly_stalled_coords = if stalled_flag {
                Some((chunk.slice_idx, chunk.group_idx))
            } else { None };

            (group_ready, needs_fec, is_nack_recovery, newly_stalled_coords)
        };

        // Слайс ещё не готов целиком или уже декодирован — выходим.
        if !group_ready || self.decoded_slices[slice_idx].is_some() {
            return (recoveries, None, is_nack_recovery, newly_stalled);
        }

        // Проверяем, готов ли весь слайс (все группы ready).
        if !self.slices.get(&chunk.slice_idx).map_or(false, |sb| sb.is_ready()) {
            return (recoveries, None, is_nack_recovery, newly_stalled);
        }

        if needs_fec {
            // ── Медленный путь: FEC нужен — отдаём в пул потоков ────────────
            if let Some(task) = self.make_decode_task(chunk.frame_id, chunk.slice_idx) {
                maybe_task = Some(task);
            }
        } else {
            // ── Быстрый путь: все data-шарды на месте, декодируем здесь ─────
            if let Some(video_slice) = self.decode_slice_sync(chunk.slice_idx) {
                self.decoded_slices[slice_idx] = Some(video_slice);
            }
            let new_packets = self.try_emit_ordered();
            self.ready_packets.extend(new_packets);
        }

        (recoveries, maybe_task, is_nack_recovery, newly_stalled)
    }

    /// Принять результат асинхронного декодирования от `ComputePool`.
    ///
    /// Возвращает `true`, если после этого фрейм стал полным.
    pub(crate) fn apply_decode_result(&mut self, slice_idx: u8, data: Vec<u8>) -> bool {
        let idx = slice_idx as usize;
        if idx >= self.decoded_slices.len() || self.decoded_slices[idx].is_some() {
            return self.is_complete();
        }

        // Десериализуем VideoSlice из собранных данных группы/слайса.
        if let Ok((video_slice, _)) = postcard::take_from_bytes::<VideoSlice>(&data) {
            self.decoded_slices[idx] = Some(video_slice);
        }

        let new_packets = self.try_emit_ordered();
        self.ready_packets.extend(new_packets);

        self.is_complete()
    }

    /// Найти первую группу в состоянии, требующем NACK, и вернуть её
    /// координаты + маску принятых шардов.
    ///
    /// Это и есть «Lazy NACK»: `FrameAssembler` не сканирует все группы —
    /// он просто спрашивает каждый `FrameBuilder`, есть ли у него
    /// «заявка» на NACK.
    pub(crate) fn find_nack_candidate(
        &mut self,
        now_us: u64,
    ) -> Option<(u8 /* slice_idx */, u8 /* group_idx */, u64 /* received_mask */)> {
        let age_us = now_us.saturating_sub(self.first_us);
        if age_us < MIN_NACK_AGE_US {
            return None;
        }

        for slice_idx in 0..self.total_slices {
            let sb = match self.slices.get_mut(&slice_idx) {
                Some(sb) => sb,
                None => continue,
            };
            if sb.is_ready() {
                continue;
            }
            for group_idx in 0..sb.total_groups {
                let gb = match sb.groups.get_mut(&group_idx) {
                    Some(gb) => gb,
                    None => continue,
                };
                if gb.state == GroupState::Ready {
                    continue;
                }
                // Обновляем стейт перед проверкой.
                gb.refresh_state(now_us);

                if gb.state.needs_nack(now_us) {
                    let mask = gb.received_mask();
                    // Переводим в Requested.
                    gb.state = GroupState::Requested { sent_at_us: now_us };
                    return Some((slice_idx, group_idx, mask));
                }
            }
        }

        None
    }

    /// Drain all staged packets if the frame is fully assembled.
    pub(crate) fn take_packets(&mut self) -> Option<Vec<VideoPacket>> {
        if self.is_complete() {
            Some(std::mem::take(&mut self.ready_packets))
        } else {
            None
        }
    }

    // ── Внутренние вспомогательные методы ────────────────────────────────────

    fn decode_slice_sync(&self, slice_idx: u8) -> Option<VideoSlice> {
        let sb = self.slices.get(&slice_idx)?;
        let data = sb.get_data_sync()?;
        let (video_slice, _): (VideoSlice, _) = postcard::take_from_bytes(&data).ok()?;
        Some(video_slice)
    }

    fn make_decode_task(&self, frame_id: u64, slice_idx: u8) -> Option<DecodeTask> {
        let sb = self.slices.get(&slice_idx)?;
        // Собираем шарды всех групп слайса для асинхронного декодирования.
        // Для простоты отдаём задачу по первой незавершённой группе;
        // полный слайс восстанавливается через `get_data_sync` из результатов.
        //
        // Реальный проект может разбивать на задачу-на-группу,
        // здесь мы передаём задачу уровня слайса.
        let (k, m, shards, payload_lens) = sb.snapshot_for_fec()?;
        Some(DecodeTask {
            frame_id,
            slice_idx,
            group_idx: 0, // не используется на приёмной стороне задачи
            k,
            m,
            shards,
            payload_lens,
        })
    }

    pub(crate) fn try_emit_ordered(&mut self) -> Vec<VideoPacket> {
        let mut packets = Vec::new();

        while self.next_emit_idx < self.total_slices {
            let idx = self.next_emit_idx as usize;

            match self.decoded_slices[idx].take() {
                Some(slice) => {
                    packets.push(Self::slice_to_packet(slice, self.first_us));
                    self.next_emit_idx += 1;
                }
                None => break,
            }
        }

        packets
    }

    fn slice_to_packet(slice: VideoSlice, first_us: u64) -> VideoPacket {
        let mut trace = slice.trace;
        if let Some(ref mut t) = trace {
            t.receive_us = first_us;
            t.reassembled_us = FrameTrace::now_us();
        }

        VideoPacket {
            frame_id:  slice.frame_id,
            payload:   slice.payload,
            slice_idx: slice.slice_idx,
            is_key:    slice.is_key,
            is_last:   slice.is_last,
            trace,
        }
    }
}

// ── SliceBuilder ─────────────────────────────────────────────────────────────

pub(crate) struct SliceBuilder {
    pub(crate) groups: HashMap<u8, GroupBuilder>,
    pub(crate) total_groups: u8,
    ready_groups: u8,
}

impl SliceBuilder {
    pub(crate) fn new(total_groups: u8) -> Self {
        Self {
            groups: HashMap::new(),
            total_groups: total_groups.max(1),
            ready_groups: 0,
        }
    }

    pub(crate) fn insert(&mut self, chunk: &DatagramChunk) -> (Option<GroupRecovery>, bool, bool) {
        if chunk.total_groups > 0 {
            self.total_groups = chunk.total_groups;
        }

        let now_us = FrameTrace::now_us();

        let group = self
            .groups
            .entry(chunk.group_idx)
            .or_insert_with(|| GroupBuilder::new(chunk.k, chunk.m));

        let was_ready = group.is_ready();
        let (was_nack_recovery, newly_stalled) = group.insert(chunk.shard_idx, chunk.data.clone(), chunk.payload_len, now_us);

        let recovery = if !was_ready && group.is_ready() {
            self.ready_groups = self.ready_groups.saturating_add(1);

            let data_present = group.data_received_count();
            let missing_data = group.k.saturating_sub(data_present);

            Some(if missing_data == 0 {
                GroupRecovery::Direct
            } else {
                GroupRecovery::Fec { missing_data_shards: missing_data }
            })
        } else {
            None
        };

        (recovery, was_nack_recovery, newly_stalled)
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready_groups == self.total_groups
    }

    pub(crate) fn group_received_mask(&self, group_idx: u8) -> u64 {
        self.groups
            .get(&group_idx)
            .map_or(0, |gb| gb.received_mask())
    }

    /// Синхронная сборка данных слайса (только fast-path, все data-шарды есть).
    pub(crate) fn get_data_sync(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        for g in 0..self.total_groups {
            let part = self.groups.get(&g)?.get_data()?;
            out.extend_from_slice(&part);
        }
        Some(out)
    }

    /// Снимок шардов первой незавершённой группы для передачи в `ComputePool`.
    pub(crate) fn snapshot_for_fec(&self) -> Option<(u8, u8, Vec<Option<Bytes>>, Vec<u16>)> {
        for g in 0..self.total_groups {
            if let Some(gb) = self.groups.get(&g) {
                if gb.state != GroupState::Ready {
                    return Some((gb.k, gb.m, gb.shards.clone(), gb.payload_lens.clone()));
                }
            }
        }
        None
    }
}

// ── GroupBuilder ─────────────────────────────────────────────────────────────

pub(crate) struct GroupBuilder {
    pub(crate) shards: Vec<Option<Bytes>>,
    pub(crate) payload_lens: Vec<u16>,
    pub(crate) received: u8,
    received_mask: u64,
    pub(crate) k: u8,
    pub(crate) m: u8,
    /// Стейт-машина группы.
    pub(crate) state: GroupState,
    /// Момент прихода первого шарда (для `MIN_NACK_AGE_US`).
    pub(crate) first_us: u64,
}

impl GroupBuilder {
    pub(crate) fn new(k: u8, m: u8) -> Self {
        let total = (k + m) as usize;
        let mut shards = Vec::with_capacity(total);
        shards.resize_with(total, || None);

        Self {
            shards,
            payload_lens: vec![0u16; k as usize],
            received: 0,
            received_mask: 0,
            k,
            m,
            state: GroupState::Collecting,
            first_us: 0,
        }
    }

    pub(crate) fn insert(&mut self, idx: u8, data: Bytes, payload_len: u16, now_us: u64) -> (bool, bool) {
        let i = idx as usize;
        if i >= self.shards.len() {
            return (false, false);
        }

        if self.shards[i].is_none() {
            self.received = self.received.saturating_add(1);
            if i < 64 {
                self.received_mask |= 1u64 << i;
            }
            if self.first_us == 0 {
                self.first_us = now_us;
            }
        }

        self.shards[i] = Some(data);
        if i < self.k as usize {
            self.payload_lens[i] = payload_len;
        }

        let was_awaited = matches!(self.state, GroupState::Requested { .. });
        let mut newly_stalled = false;

        // Ленивый и умный переход состояний
        match self.state {
            GroupState::Ready | GroupState::Abandoned | GroupState::Decoding => {}
            _ => {
                let total = (self.k + self.m) as u64;
                let missing = total.saturating_sub(self.received as u64);
                let can_fec = self.m as u64;

                if missing > can_fec {
                    // Группа пробила лимит потерь — кричим ассемблеру добавить нас в очередь
                    if self.state == GroupState::Collecting {
                        self.state = GroupState::Stalled { since_us: now_us };
                        newly_stalled = true;
                    }
                } else {
                    // FEC снова справляется (например, долетел нужный пакет) — откатываемся
                    if matches!(self.state, GroupState::Stalled { .. } | GroupState::Requested { .. }) {
                        self.state = GroupState::Collecting;
                    }
                }
            }
        }
        
        (was_awaited, newly_stalled)
    }

    /// Пересчитать стейт группы на основе текущего числа принятых шардов.
    ///
    /// Вызывается:
    /// - после каждого `insert`,
    /// - из `find_nack_candidate` перед проверкой `needs_nack`.
    pub(crate) fn refresh_state(&mut self, now_us: u64) {
        // Уже финальное состояние — не трогаем.
        match self.state {
            GroupState::Ready | GroupState::Abandoned => return,
            _ => {}
        }

        if self.is_ready() {
            self.state = GroupState::Ready;
            return;
        }

        // Сколько шардов ещё не пришло?
        let total    = (self.k + self.m) as u64;
        let missing  = total.saturating_sub(self.received as u64);
        // FEC может восстановить до M пропаж.
        let can_fec  = self.m as u64;

        if missing > can_fec {
            // FEC не справится: Collecting → Stalled (только при первом переходе).
            if self.state == GroupState::Collecting {
                self.state = GroupState::Stalled { since_us: now_us };
            }
            // Если уже Requested — даём NACK_SUPPRESS_US истечь;
            // `needs_nack` вернёт true когда время придёт.
        } else {
            // FEC справится или осталось ждать — откатываемся в Collecting
            // (актуально, если ретрансмиссия вернула нужный шард).
            if matches!(self.state, GroupState::Stalled { .. } | GroupState::Requested { .. }) {
                self.state = GroupState::Collecting;
            }
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.received >= self.k
    }

    pub(crate) fn received_mask(&self) -> u64 {
        self.received_mask
    }

    pub(crate) fn get_data(&self) -> Option<Vec<u8>> {
        FecDecoder::decode(self.k, self.m, &self.shards, &self.payload_lens)
    }

    pub(crate) fn data_received_count(&self) -> u8 {
        let mut c = 0u8;
        for i in 0..self.k as usize {
            if self.shards[i].is_some() {
                c += 1;
            }
        }
        c
    }
}