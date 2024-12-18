mod delta_g;

use ngrams::Ngram;
use seq_io::fasta::{Reader, Record};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::io::{self, BufReader};
use itertools::Itertools;
use std_dev::standard_deviation;

const KMER_SIZE: usize = 13;
const OVLP_WINDOWS_SIZE: usize = 250;
const MAX_MISMATCH_SEGMENTS: usize = 0;
const MAX_ITERATIONS: usize = 100;
const SEARCH_WINDOWS_SIZE: usize = 50;

const MV_CONC: f64 = 50.0; // Monovalent cation concentration (mM)
const DV_CONC: f64 = 0.0; // Divalent cation concentration (mM)
const DNTP_CONC: f64 = 0.8; // dNTP concentration (mM)
const DNA_CONC: f64 = 50.0; // Primer concentration (nM)
const ANNEALING_TEMP: f64 = 45.0; // Annealing temperature (°C)

struct SequenceRecord {
    name: String,
    sequence: String,
}

const SEQ_DIR_FWD: u8 = 0x00;
const SEQ_DIR_REV: u8 = 0x01;

#[derive(Hash, Clone)]
struct KmerRecord {
    word: String,
    direction: u8,
}

impl PartialEq for KmerRecord {
    fn eq(&self, other: &Self) -> bool {
        self.word == other.word && self.direction == other.direction
    }
}

impl Eq for KmerRecord {}

#[derive(Clone)]
struct KmerFrequency {
    kmer: KmerRecord,
    frequency: usize,
}

impl PartialEq for KmerFrequency {
    fn eq(&self, other: &Self) -> bool {
        self.kmer.word == other.kmer.word
    }
}

impl Eq for KmerFrequency {}

impl Hash for KmerFrequency {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.kmer.word.hash(state);
    }
}

struct KmerStat {
    word: String,
    direction: u8,
    frequency: usize,
    gc_percent: f32,
    tm: f32,
    tm_ok: bool,
    repeats: bool,
    runs: bool,
    delta_g: f32,
    hairpin: bool,
}

struct Segment {
    index: u16,
    kmers: HashMap<KmerRecord, usize>,
}

type KmerSegmentMapping = HashMap<String, HashMap<usize, u32>>;

struct FindingCandidateKmersContext<'a> {
    segments: &'a HashMap<usize, Segment>,
    skipped_segment_indexes: &'a mut HashSet<usize>,
    kmer_segments_mapping: &'a KmerSegmentMapping,
}

fn to_records(src: Vec<u8>) -> io::Result<Vec<SequenceRecord>> {
    let mut reader = Reader::new(BufReader::new(src.as_slice()));
    let mut records = Vec::new();

    while let Some(result) = reader.next() {
        let record = result.unwrap();
        let name = record.id().unwrap().to_string();
        let sequence = String::from_utf8(record.full_seq().to_vec())
            .unwrap()
            .to_uppercase()
            .replace("U", "T");
        records.push(SequenceRecord { name, sequence });
    }
    Ok(records)
}

/**
 * Aligns sequences using MAFFT
 */
fn align_sequences(filepath: String) -> Result<Vec<u8>, io::Error> {
    let output = std::process::Command::new("mafft")
        .args(["--auto", "--quiet", "--thread", "-1", &filepath.clone()])
        .output()
        .expect("failed to execute MAFFT");

    Ok(output.stdout)
}

fn reverse_complement(sequence: &str) -> String {
    sequence
        .chars()
        .rev()
        .map(|c| match c {
            'A' => 'T',
            'T' => 'A',
            'U' => 'A',
            'C' => 'G',
            'G' => 'C',
            _ => c,
        })
        .collect()
}

fn find_kmers(sequence: &str, kmer_size: usize) -> Vec<String> {
    sequence
        .chars()
        .ngrams(kmer_size)
        .filter(|kmer| kmer.iter().all(|c| !"Nn- ".contains(*c)))
        .map(|kmer| kmer.iter().collect())
        .unique()
        .collect()
}

fn partitioning_sequence(sequence: &str, ovlp_windows_size: usize) -> Vec<String> {
    sequence
        .chars()
        .collect::<Vec<char>>()
        .windows(ovlp_windows_size * 2)
        .step_by(ovlp_windows_size)
        .map(|window| window.iter().collect())
        .collect()
}

fn get_sequence_on_search_windows(sequence: &String, search_windows_size: usize) -> (String, String) {
    let first = &sequence[..search_windows_size];
    let second = &sequence[sequence.len() - search_windows_size..];
    (first.to_string(), second.to_string())
}

fn get_segments(records: &Vec<SequenceRecord>, ovlp_windows_size: usize, kmer_size: usize, search_windows_size: usize) -> HashMap<usize, Segment> {
    let mut segments: HashMap<usize, Segment> = HashMap::new();

    if ovlp_windows_size < search_windows_size {
        panic!("Overlap windows size must be greater or equal than search windows size");
    }

    for (seq_id, rec) in records.iter().enumerate() {
        // 1. Partitioning the sequence into segments
        let partitions = partitioning_sequence(&rec.sequence, ovlp_windows_size);
        log::debug!("seq_id={}, partitions={}, first_part_len={}", seq_id, partitions.len(), partitions[0].len());
        // 2. Extracting forward k-mers from each segment
        for (idx, partition) in partitions.iter().enumerate() {
            // Find the segment at idx, if not exists, create a new one
            let segment = segments.entry(idx).or_insert(Segment {
                index: idx as u16,
                kmers: HashMap::new(),
            });
            // Push the k-mers into the list
            let (fwd_windows, rev_windows) = get_sequence_on_search_windows(&partition, search_windows_size);
            let fwd_kmers: Vec<KmerRecord> = find_kmers(&*fwd_windows, kmer_size)
                .iter()
                .map(|kmer| KmerRecord {
                    word: kmer.to_string(),
                    direction: SEQ_DIR_FWD,
                })
                .collect();
            let rev_kmers: Vec<KmerRecord> = find_kmers(&*reverse_complement(&rev_windows), kmer_size)
                .iter()
                .map(|kmer| KmerRecord {
                    word: kmer.to_string(),
                    direction: SEQ_DIR_REV,
                })
                .collect();

            for kmer in fwd_kmers.iter() {
                segment
                    .kmers
                    .entry(kmer.clone())
                    .and_modify(|e| *e += 1)
                    .or_insert(1);
            }
            for kmer in rev_kmers.iter() {
                segment
                    .kmers
                    .entry(kmer.clone())
                    .and_modify(|e| *e += 1)
                    .or_insert(1);
            }
        }
    }

    segments
}

fn make_kmer_segments_mapping(segments: &HashMap<usize, Segment>) -> KmerSegmentMapping {
    let mut kmer_segments_mapping: KmerSegmentMapping = HashMap::new();
    for (i, segment) in segments.iter() {
        for (kmer, freq) in segment.kmers.iter() {
            let rec = KmerRecord {
                word: kmer.word.clone(),
                direction: kmer.direction,
            };
            kmer_segments_mapping
                .entry(rec.word.clone())
                .or_insert(HashMap::new())
                .insert(*i, *freq as u32);
            log::trace!("kmer={}, freq={}, segment={}", rec.word.clone(), freq, i);
        }
    }
    kmer_segments_mapping
}

fn find_most_freq_kmer<'a>(
    segments: &'a HashMap<usize, Segment>,
    kmer_segments_mapping: &'a KmerSegmentMapping,
    skipped_segment_indexes: HashSet<usize>,
) -> Option<(KmerFrequency, HashSet<usize>)> {
    let mut kmer_total_counts: HashMap<&KmerRecord, usize> = HashMap::new();
    let mut candidate_kmers: HashMap<usize, KmerFrequency> = HashMap::new();
    for (_, segment) in segments.iter() {
        if skipped_segment_indexes.contains(&(segment.index as usize)) {
            continue;
        }

        let mut kmer_map: HashMap<String, &KmerRecord> = HashMap::new();
        let mut kmer_counts: HashMap<String, usize> = HashMap::new();
        for (kmer_record, counts) in segment.kmers.iter() {
            kmer_map.entry(kmer_record.word.clone()).or_insert(kmer_record);
            kmer_counts.entry(kmer_record.word.clone()).and_modify(|e| *e += counts).or_insert(*counts);
            kmer_total_counts
                .entry(kmer_record)
                .and_modify(|e| *e += counts)
                .or_insert(*counts);
        }
        let (word, freq) = kmer_counts.into_iter().max_by(|(_, a), (_, b)| a.cmp(b)).unwrap();
        let winner_kmer = kmer_map.get(&word).unwrap();
        candidate_kmers.insert(
            segment.index as usize,
            KmerFrequency {
                kmer: KmerRecord {
                    word: word.clone(),
                    direction: winner_kmer.direction,
                },
                frequency: freq,
            },
        );
    }

    if candidate_kmers.len() == 0 {
        return None;
    }

    let (_, winner) = candidate_kmers
        .iter()
        .max_by(|(_, a), (_, b)| {
            let a_freq = kmer_total_counts.get(&a.kmer).unwrap();
            let b_freq = kmer_total_counts.get(&b.kmer).unwrap();
            a_freq.cmp(b_freq)
        })
        .unwrap();

    let winner_kmer_segments = kmer_segments_mapping.get(&winner.kmer.word.clone());
    let matched_segment_indexes: Vec<usize> = winner_kmer_segments.unwrap().keys().map(|k| *k).collect();

    let mut skipped_segment_indexes: HashSet<usize> = HashSet::new();
    for idx in matched_segment_indexes {
        skipped_segment_indexes.insert(idx);
    }
    let winner_kmer_segments_rev = kmer_segments_mapping.get(&reverse_complement(&winner.kmer.word.clone()));
    if winner_kmer_segments_rev.is_some() {
        let matched_segment_indexes_rev: Vec<usize> = winner_kmer_segments_rev
            .unwrap()
            .keys()
            .map(|k| *k)
            .collect();
        for idx in matched_segment_indexes_rev {
            skipped_segment_indexes.insert(idx);
        }
    }

    Some((
        KmerFrequency {
            kmer: KmerRecord {
                word: winner.kmer.word.clone(),
                direction: winner.kmer.direction,
            },
            frequency: winner.frequency,
        },
        skipped_segment_indexes,
    ))
}

mod tests {
    use super::*;

    #[test]
    fn test_reverse_complement() {
        let sequence = "ATCGAA";
        assert_eq!(reverse_complement(sequence), "TTCGAT");
    }

    #[test]
    fn test_get_search_windows() {
        let sequence = "AACCTTGGAACCTTG-".to_string();
        let (first, second) = get_sequence_on_search_windows(&sequence, 5);
        assert_eq!(first, "AACCT");
        assert_eq!(second, "CTTG-");
    }

    #[test]
    fn test_find_kmers() {
        let sequence = "AACCTTGGAACCTTG-";
        let kmers = find_kmers(sequence, 5);
        assert_eq!(kmers.len(), 8);
        assert!(kmers.contains(&"AACCT".to_string()));
        assert!(kmers.contains(&"ACCTT".to_string()));
        assert!(kmers.contains(&"CCTTG".to_string()));
        assert!(kmers.contains(&"CTTGG".to_string()));
        assert!(kmers.contains(&"TTGGA".to_string()));
        assert!(kmers.contains(&"TGGAA".to_string()));
        assert!(kmers.contains(&"GGAAC".to_string()));
        assert!(kmers.contains(&"GAACC".to_string()));
    }

    #[test]
    fn test_get_segments() {
        // search windows = (AACCT)(TGGAA)
        // AAC, freq=2
        // ACC, freq=3
        // CCT, freq=3
        // reverse complement = AAGGT -> TTCCA
        // TTC, freq=3
        // TCC, freq=3
        // CCA, freq=3
        let records = vec![
            SequenceRecord {
                name: "seq1".to_string(),
                sequence: "AACCTTGGAACCTTGG".to_string(),
            },
            SequenceRecord {
                name: "seq2".to_string(),
                sequence: "AACCTTGGAACCTTG-".to_string(),
            },
            SequenceRecord {
                name: "seq3".to_string(),
                sequence: "-ACCTTGGAACCTT-G".to_string(),
            },
        ];
        for rec in records.iter() {
            println!("seq={} ---", rec.name);
            for p in partitioning_sequence(&rec.sequence, 5) {
                println!("-- partition={}", p);
                let (i, j) = get_sequence_on_search_windows(&p, 5);
                println!("---- search_windows(start)={}", i);
                println!("---- search_windows(end)={}", j);
                let rev = reverse_complement(&j);
                println!("---- search_windows(end/rev)={}", rev);
            }
        }
        let segments = get_segments(&records, 5, 3, 5);
        assert_eq!(segments.len(), 2);
        let first_segment = segments.get(&0).unwrap();
        for (kmer, freq) in first_segment.kmers.iter() {
            println!("kmer={}, direction={}, freq={}", kmer.word, kmer.direction, freq);
        }
        assert_eq!(segments.get(&0).unwrap().kmers.len(), 6);
        let aac_kmer = KmerRecord { word: "AAC".to_string(), direction: 0 };
        let ttc_kmer = KmerRecord { word: "TTC".to_string(), direction: 1 };
        assert_eq!(*first_segment.kmers.get(&aac_kmer).unwrap(), 2usize);
        assert_eq!(*first_segment.kmers.get(&ttc_kmer).unwrap(), 3usize);
        assert_eq!(segments.get(&1).unwrap().kmers.len(), 6);
    }

    #[test]
    fn test_partitioning_sequence() {
        let sequence = "AACCTTGGAACCTTGG";
        let partitions = partitioning_sequence(sequence, 5);
        assert_eq!(partitions.len(), 2);
        assert_eq!(partitions[0], "AACCTTGGAA");
        assert_eq!(partitions[1], "TGGAACCTTG");
    }

    #[test]
    fn test_make_kmer_segments_mapping() {
        let segments: HashMap<usize, Segment> = {
            let mut segments: HashMap<usize, Segment> = HashMap::new();

            let mut kmers = HashMap::new();
            kmers.insert(KmerRecord { word: "ACTG".to_string(), direction: SEQ_DIR_FWD }, 1);
            kmers.insert(KmerRecord { word: "AGGT".to_string(), direction: SEQ_DIR_FWD }, 1);
            kmers.insert(KmerRecord { word: "ATTA".to_string(), direction: SEQ_DIR_FWD }, 2);
            segments.insert(0, Segment {
                index: 0,
                kmers,
            });

            kmers = HashMap::new();
            kmers.insert(KmerRecord { word: "GCAT".to_string(), direction: SEQ_DIR_FWD }, 1);
            kmers.insert(KmerRecord { word: "AGGT".to_string(), direction: SEQ_DIR_FWD }, 2);
            kmers.insert(KmerRecord { word: "GGAA".to_string(), direction: SEQ_DIR_FWD }, 1);
            segments.insert(1, Segment {
                index: 1,
                kmers,
            });

            segments
        };
        let kmer_segments_mapping = make_kmer_segments_mapping(&segments);
        assert_eq!(kmer_segments_mapping.len(), 5);
        assert_eq!(kmer_segments_mapping.get("ACTG").unwrap().len(), 1);
        assert_eq!(kmer_segments_mapping.get("AGGT").unwrap().len(), 2);
        assert_eq!(kmer_segments_mapping.get("ATTA").unwrap().len(), 1);
        assert_eq!(kmer_segments_mapping.get("GCAT").unwrap().len(), 1);
        assert_eq!(kmer_segments_mapping.get("GGAA").unwrap().len(), 1);
    }

    #[test]
    fn test_find_most_freq_kmer() {
        let segments: HashMap<usize, Segment> = {
            let mut segments: HashMap<usize, Segment> = HashMap::new();

            let mut kmers = HashMap::new();
            kmers.insert(KmerRecord { word: "ACTG".to_string(), direction: SEQ_DIR_FWD }, 1);
            kmers.insert(KmerRecord { word: "AGGT".to_string(), direction: SEQ_DIR_FWD }, 1);
            kmers.insert(KmerRecord { word: "ATTA".to_string(), direction: SEQ_DIR_FWD }, 2);
            segments.insert(0, Segment {
                index: 0,
                kmers,
            });

            kmers = HashMap::new();
            kmers.insert(KmerRecord { word: "GCAT".to_string(), direction: SEQ_DIR_FWD }, 1);
            kmers.insert(KmerRecord { word: "AGGT".to_string(), direction: SEQ_DIR_FWD }, 2);
            kmers.insert(KmerRecord { word: "GGAA".to_string(), direction: SEQ_DIR_FWD }, 1);
            segments.insert(1, Segment {
                index: 1,
                kmers,
            });

            kmers = HashMap::new();
            kmers.insert(KmerRecord { word: "GGGG".to_string(), direction: SEQ_DIR_FWD }, 1);
            kmers.insert(KmerRecord { word: "ATGA".to_string(), direction: SEQ_DIR_FWD }, 2);
            kmers.insert(KmerRecord { word: "TTTT".to_string(), direction: SEQ_DIR_FWD }, 1);
            segments.insert(2, Segment {
                index: 2,
                kmers,
            });

            segments
        };
        let kmer_segments_mapping: KmerSegmentMapping = make_kmer_segments_mapping(&segments);
        let mut skipped_segment_indexes: HashSet<usize> = HashSet::new();

        let result = find_most_freq_kmer(&segments, &kmer_segments_mapping, skipped_segment_indexes.clone());
        assert_eq!(result.is_some(), true);
        let (kmer, skipped) = result.unwrap();
        assert_eq!(kmer.kmer.word, "AGGT");
        assert_eq!(kmer.frequency, 2);
        assert_eq!(skipped.len(), 2);

        for idx in skipped.iter() {
            skipped_segment_indexes.insert(*idx);
        }
        let result = find_most_freq_kmer(&segments, &kmer_segments_mapping, skipped_segment_indexes.clone());
        assert_eq!(result.is_some(), true);
        let (kmer, skipped) = result.unwrap();
        assert_eq!(kmer.kmer.word, "ATGA");
        assert_eq!(kmer.frequency, 2);
        assert_eq!(skipped.len(), 1);
    }
}

fn get_candidates_kmers(segments: &HashMap<usize, Segment>) -> Vec<KmerFrequency> {
    let mut skipped_segment_indexes: HashSet<usize> = HashSet::new();
    let mut candidate_kmers: Vec<KmerFrequency> = Vec::new();
    let kmer_segment_mappings = make_kmer_segments_mapping(&segments);

    let total_segments = segments.iter().len();

    for iter_no in 0..MAX_ITERATIONS {
        let result = find_most_freq_kmer(
            &segments,
            &kmer_segment_mappings,
            skipped_segment_indexes.clone()
        );
        if result.is_none() {
            log::debug!("Iteration={}, No candidate k-mers found", iter_no);
            break;
        }
        let (iter_winner, new_skipped_indexes) = result.unwrap();
        log::debug!("Iteration={}, winner: {}, freq={}, skips={}", iter_no, iter_winner.kmer.word, iter_winner.frequency, new_skipped_indexes.len());
        for idx in new_skipped_indexes.iter() {
            skipped_segment_indexes.insert(*idx);
        }

        let remaining_segments = total_segments - skipped_segment_indexes.len();
        if remaining_segments < MAX_MISMATCH_SEGMENTS {
            break;
        }
        candidate_kmers.push(iter_winner);
    }

    candidate_kmers.iter().map(|k| KmerFrequency {
        kmer: KmerRecord {
            word: k.kmer.word.clone(),
            direction: k.kmer.direction,
        },
        frequency: k.frequency,
    }).collect()
}

fn get_kmer_stats(kmer_records: Vec<KmerFrequency>) -> Vec<KmerStat> {
    // first, finding the threshold for Tm
    let primers: Vec<String> = kmer_records.iter().map(|k| k.kmer.word.clone()).collect();
    let tm_threshold = get_tm_threshold(primers);

    kmer_records
        .iter()
        .map(|kmer_freq| {
            let freq = kmer_freq.frequency;
            let tm = get_tm(kmer_freq.kmer.word.clone());
            let delta_g = 0.0;
            KmerStat {
                word: kmer_freq.kmer.word.clone(),
                direction: kmer_freq.kmer.direction,
                frequency: freq,
                gc_percent: get_gc_percent(kmer_freq.kmer.word.clone()),
                tm,
                tm_ok: in_tm_threshold(kmer_freq.kmer.word.clone(), tm_threshold),
                repeats: is_repeats(kmer_freq.kmer.word.clone()),
                runs: is_run(kmer_freq.kmer.word.clone()),
                delta_g,
                hairpin: delta_g < -9.0,
            }
        })
        .collect()
}

fn get_gc_percent(kmer: String) -> f32 {
    let mut gc_count: f32 = 0.0;
    for c in kmer.chars() {
        if c == 'G' || c == 'C' {
            gc_count += 1.0;
        }
    }
    (gc_count / kmer.len() as f32) * 100.0
}

fn get_tm(kmer: String) -> f32 {
    let mut gc_count: f32 = 0.0;
    let mut at_count: f32 = 0.0;
    for c in kmer.chars() {
        if c == 'G' || c == 'C' {
            gc_count += 1.0;
        }
        if c == 'A' || c == 'T' {
            at_count += 1.0;
        }
    }
    64.9 + (41.0 * (gc_count - 16.4) / (gc_count + at_count))
}

/**
 * Get the threshold for Tm
 *
 * Calculated by find (2*sd(Tm)) + mean(Tm) of the primers
 */
fn get_tm_threshold(primers: Vec<String>) -> f32 {
    let mut tm_values: Vec<f32> = Vec::new();
    for primer in primers {
        tm_values.push(get_tm(primer));
    }
    let mean_tm = tm_values.iter().sum::<f32>() / tm_values.len() as f32;
    let sd_tm = standard_deviation(&tm_values);
    (2.0 * sd_tm.standard_deviation) + mean_tm
}

fn in_tm_threshold(kmer: String, threshold: f32) -> bool {
    get_tm(kmer) < threshold
}

/**
 * Check if the kmer  has >=5nt di-nucleotide repeats
 *
 * For example, ATATATATATGG is too many AT repeats, then return `true`.
 */
fn is_repeats(kmer: String) -> bool {
    let kmer_ngrams = kmer.chars().ngrams(2);
    let mut repeats = 0;
    let mut last_chunk: Vec<char> = Vec::new();
    for chunk in kmer_ngrams {
        if chunk == last_chunk {
            repeats += 1;
        } else {
            repeats = 0;
        }
        last_chunk = chunk;
    }
    repeats >= 5
}

fn is_run(kmer: String) -> bool {
    let mut runs = 0;
    let mut last_char = ' ';
    for c in kmer.chars() {
        if c == last_char {
            runs += 1;
        } else {
            runs = 0;
        }
        last_char = c;
    }
    runs >= 5
}

fn main() -> io::Result<()> {
    env_logger::init();

    let filename = String::from("samples.fasta");

    // 1. Align sequences
    log::debug!("Aligning sequences...");
    let records = match align_sequences(filename) {
        Ok(records) => to_records(records)?,
        Err(e) => {
            panic!("Error aligning sequences: {}", e);
        }
    };
    log::debug!("Done aligning sequences");
    if records.iter().len() == 0 {
        panic!("No sequences found in the input file");
    }

    // 2. Extracting n-grams from each sequence segments
    log::debug!("Extracting n-grams from each sequence segments...");
    let segments = get_segments(&records, OVLP_WINDOWS_SIZE, KMER_SIZE, SEARCH_WINDOWS_SIZE);
    log::debug!("Done, Total segments: {}", segments.len());

    // 3. Calculate frequencies of n-grams for each segment both forward/reverse
    log::debug!("Calculating frequencies of k-mer for all segments...");
    let candidate_kmers = get_candidates_kmers(&segments);
    log::debug!(
        "Done calculating, Total k-mers left: {}",
        candidate_kmers.len()
    );

    // 4. Filtering out unmatched criteria
    log::debug!("Filtering out unmatched criteria (Tm and >5nt repeats, runs...)");
    let kmer_stats = get_kmer_stats(candidate_kmers);
    let candidate_primers: Vec<&KmerStat> = kmer_stats
        .iter()
        .filter(|kmer_stat| {
            kmer_stat.tm_ok
                && !kmer_stat.repeats
                && !kmer_stat.runs
                && kmer_stat.gc_percent >= 40.0
                && kmer_stat.gc_percent <= 60.0
        })
        .collect();
    log::debug!("Done filtering out unmatched, primers left = {}", candidate_primers.len());

    // 5. Output the primers
    log::debug!("Outputting primers...");
    let output_file = "output/primers.csv";
    let mut writer = csv::Writer::from_path(output_file)?;
    writer.write_record(&["name", "sequence", "species_name", "tax_id"])?;
    for (idx, primer) in candidate_primers.iter().enumerate() {
        let name_suffix = if primer.direction == SEQ_DIR_FWD {
            "F"
        } else {
            "R"
        };
        writer.write_record(&[
            format!("primer_{}_{}", idx, name_suffix),
            primer.word.clone(),
            // hard code for now.
            "Some virus".to_string(),
            "12345".to_string(),
        ])?;
    }
    writer.flush().expect("Error writing output to file");
    log::debug!("Done outputting primers");

    Ok(())
}
