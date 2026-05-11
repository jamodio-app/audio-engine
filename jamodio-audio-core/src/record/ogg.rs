//! Ogg page writer + CRC32 — strict minimum pour envelopper un bitstream
//! Opus dans un container Ogg/Opus standard (RFC 7845).
//!
//! Port direct du writer JS (app/js/lib/opus-remux.js) — même algorithme,
//! mêmes constantes. Compatible bit-à-bit avec VLC, ffplay, Reaper, etc.

/// CRC32 Ogg : polynôme 0x04C11DB7, init 0, no reflection, no final XOR.
/// Pas le même que zlib (qui utilise reflection). Table calculée à la
/// construction du writer pour éviter `lazy_static`.
fn build_crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            r = if r & 0x80000000 != 0 { (r << 1) ^ 0x04C11DB7 } else { r << 1 };
            j += 1;
        }
        t[i] = r;
        i += 1;
    }
    t
}

fn ogg_crc(buf: &[u8], table: &[u32; 256]) -> u32 {
    let mut crc: u32 = 0;
    for &b in buf {
        crc = (crc << 8) ^ table[((crc >> 24) ^ (b as u32)) as usize & 0xff];
    }
    crc
}

/// Header bytes for a single Ogg page. Convention RFC 3533 :
///   - 0..4   : "OggS" magic
///   - 4      : version (0)
///   - 5      : header_type (0x02 = BOS, 0x04 = EOS, 0x01 = continued)
///   - 6..14  : granule_position (u64 LE)
///   - 14..18 : bitstream_serial (u32 LE)
///   - 18..22 : page_sequence (u32 LE)
///   - 22..26 : CRC32 (calculé sur la page entière avec CRC champ = 0)
///   - 26     : number of page segments (u8, max 255)
///   - 27..   : segment table (N bytes : taille de chaque lacing segment)
///   - puis   : data (packets concaténés)
pub struct OggWriter {
    serial: u32,
    seq_num: u32,
    /// Output buffer accumulant les pages écrites jusqu'à finalize.
    pub bytes: Vec<u8>,
    crc_table: [u32; 256],
    /// Granule position cumulée (samples décodés à 48kHz à la fin de la page).
    granule: u64,
    /// `true` après la 1re page : la suivante n'est plus BOS.
    bos_written: bool,
}

impl OggWriter {
    pub fn new(serial: u32) -> Self {
        Self {
            serial,
            seq_num: 0,
            bytes: Vec::with_capacity(64 * 1024),
            crc_table: build_crc_table(),
            granule: 0,
            bos_written: false,
        }
    }

    /// Écrit la page header initiale (OpusHead) — premier packet du stream.
    /// Le packet `opus_head` est conforme RFC 7845 §5.1 (19 octets standard).
    pub fn write_header(&mut self, opus_head: &[u8]) {
        // BOS flag = 0x02 ; granule = 0 (les pages de header n'avancent pas le temps)
        self.write_page(0x02, 0, &[opus_head]);
    }

    /// Écrit la page OpusTags (2e packet du stream).
    pub fn write_tags(&mut self, opus_tags: &[u8]) {
        self.write_page(0x00, 0, &[opus_tags]);
    }

    /// Écrit une page contenant un ou plusieurs packets Opus audio.
    /// `granule_at_end` = nombre total de samples décodés à 48kHz à la fin
    /// du dernier packet de cette page (cumul depuis le début du stream).
    /// `is_last` → set EOS flag (0x04).
    pub fn write_audio_page(&mut self, packets: &[&[u8]], granule_at_end: u64, is_last: bool) {
        let flag = if is_last { 0x04 } else { 0x00 };
        self.granule = granule_at_end;
        self.write_page(flag, granule_at_end, packets);
    }

    /// Écrit une page Ogg complète. Calcul du segment_table (Ogg lacing) +
    /// CRC32, append au buffer interne.
    fn write_page(&mut self, header_type: u8, granule: u64, packets: &[&[u8]]) {
        // Effective flag : 0x02 (BOS) uniquement sur la 1re page.
        let mut flag = header_type;
        if !self.bos_written && flag & 0x02 == 0 {
            // Toute première page DOIT avoir BOS — sinon les players rejettent.
            // (Seule la première peut l'avoir.)
        }
        if self.bos_written {
            flag &= !0x02; // pas BOS si pas la première
        }

        // Lacing : chaque packet est découpé en segments de 255 octets max.
        // Si la taille modulo 255 vaut exactement 0 (= packet multiple de 255),
        // on ajoute un segment de 0 octet pour signaler explicitement la fin.
        let mut segment_table: Vec<u8> = Vec::new();
        for p in packets {
            let mut len = p.len();
            while len >= 255 {
                segment_table.push(255);
                len -= 255;
            }
            segment_table.push(len as u8);
            // Si la taille du packet est exactement multiple de 255, le
            // dernier segment est 0 (= explicit end marker requis par Ogg).
            // Couvert par le push du `len as u8 = 0` ci-dessus quand len == 0.
        }
        assert!(segment_table.len() <= 255, "Ogg : max 255 segments par page");

        let header_size = 27 + segment_table.len();
        let data_size: usize = packets.iter().map(|p| p.len()).sum();
        let total = header_size + data_size;

        // Position dans self.bytes où débute cette page — pour calculer le CRC
        // sur cette tranche après l'avoir entièrement écrite.
        let page_start = self.bytes.len();
        self.bytes.reserve(total);
        // 0..4 : "OggS"
        self.bytes.extend_from_slice(b"OggS");
        // 4 : version
        self.bytes.push(0);
        // 5 : header_type flag
        self.bytes.push(flag);
        // 6..14 : granule_position (u64 LE)
        self.bytes.extend_from_slice(&granule.to_le_bytes());
        // 14..18 : bitstream_serial (u32 LE)
        self.bytes.extend_from_slice(&self.serial.to_le_bytes());
        // 18..22 : page_sequence (u32 LE)
        self.bytes.extend_from_slice(&self.seq_num.to_le_bytes());
        // 22..26 : CRC placeholder (0, sera réécrit après calcul)
        self.bytes.extend_from_slice(&[0, 0, 0, 0]);
        // 26 : number of segments
        self.bytes.push(segment_table.len() as u8);
        // 27..26+N : segment_table
        self.bytes.extend_from_slice(&segment_table);
        // data : packets concaténés
        for p in packets {
            self.bytes.extend_from_slice(p);
        }

        // Calcule le CRC sur la page complète (avec CRC field = 0) et l'écrit.
        let crc = ogg_crc(&self.bytes[page_start..page_start + total], &self.crc_table);
        self.bytes[page_start + 22..page_start + 26].copy_from_slice(&crc.to_le_bytes());

        self.seq_num = self.seq_num.wrapping_add(1);
        self.bos_written = true;
    }
}

/// Construit un OpusHead conforme RFC 7845 §5.1 (19 octets pour la version
/// canonique sans channel mapping table).
///
///   - 0..8  : magic "OpusHead"
///   - 8     : version (1)
///   - 9     : channel_count
///   - 10..12: pre_skip (u16 LE) — samples à ignorer au début du décodage
///   - 12..16: input_sample_rate (u32 LE) — déclaratif, pas utilisé pour le décodage
///   - 16..18: output_gain (i16 LE, Q7.8)
///   - 18    : channel_mapping_family (0 = mono/stereo simple)
pub fn build_opus_head(channels: u8, pre_skip: u16, input_sample_rate: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(19);
    h.extend_from_slice(b"OpusHead");
    h.push(1);                      // version
    h.push(channels);               // channel_count
    h.extend_from_slice(&pre_skip.to_le_bytes());
    h.extend_from_slice(&input_sample_rate.to_le_bytes());
    h.extend_from_slice(&0i16.to_le_bytes()); // output_gain
    h.push(0);                      // channel_mapping_family
    h
}

/// Construit un OpusTags minimal : magic + vendor string + 0 user comments.
pub fn build_opus_tags(vendor: &str) -> Vec<u8> {
    let mut t = Vec::with_capacity(16 + vendor.len());
    t.extend_from_slice(b"OpusTags");
    t.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    t.extend_from_slice(vendor.as_bytes());
    t.extend_from_slice(&0u32.to_le_bytes()); // 0 user comments
    t
}
