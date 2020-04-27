use crate::config::Config;
use crate::errors::Error;
use crate::handle::Handle;
use crate::packet::Packet;
use crate::pcap_util;

use crate::stream::StreamItem;
use failure::Fail;
use failure::_core::iter::Peekable;
use futures::future::Pending;
use futures::stream::{Stream, StreamExt};
use log::*;
use pin_project::pin_project;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread::current;
use std::time::{Duration, SystemTime};
use tokio::time::Delay;

struct BridgeStreamState<E, T>
where
    E: Fail + Sync + Send,
    T: Stream<Item = StreamItem<E>> + Sized + Unpin,
{
    stream: T,
    current: Vec<Vec<Packet>>,
    complete: bool,
}

impl<E: Fail + Sync + Send, T: Stream<Item = StreamItem<E>> + Sized + Unpin>
    BridgeStreamState<E, T>
{
    fn is_complete(&self) -> bool {
        self.complete && self.current.is_empty()
    }

    fn spread(&self) -> Duration {
        let min = self.current.first().map(|s| s.first()).flatten();

        let max = self.current.first().map(|s| s.first()).flatten();

        match (min, max) {
            (Some(min), Some(max)) => max.timestamp().duration_since(*min.timestamp()).unwrap(),
            _ => Duration::from_millis(0),
        }
    }
}

#[pin_project]
pub struct BridgeStream<E: Fail + Sync + Send, T>
where
    T: Stream<Item = StreamItem<E>> + Sized + Unpin,
{
    stream_states: VecDeque<BridgeStreamState<E, T>>,
    max_buffer_time: Duration,
}

impl<E: Fail + Sync + Send, T: Stream<Item = StreamItem<E>> + Sized + Unpin> BridgeStream<E, T> {
    pub fn new(streams: Vec<T>, max_buffer_time: Duration) -> Result<BridgeStream<E, T>, Error> {
        let mut stream_states = VecDeque::with_capacity(streams.len());
        for stream in streams {
            let new_state = BridgeStreamState {
                stream: stream,
                current: vec![],
                complete: false,
            };
            stream_states.push_back(new_state);
        }

        Ok(BridgeStream {
            stream_states: stream_states,
            max_buffer_time,
        })
    }
}

// Playing around with using the fact that all array are already sorted, however, this is not as fast as merge sort, leaving it here in case someone wants to point out optimizations.
// fn sort_packets<I: Iterator<Item = Packet>>(mut to_sort: Vec<Peekable<I>>, size: usize) -> Vec<Packet> {
//     //let cap: usize = to_sort.iter().map(|it| it.count()).sum();
//     let mut to_return: Vec<Packet> = Vec::with_capacity(size);
//     loop {
//         let mut current_lowest: Option<(usize, SystemTime)> = None;
//         if to_sort.len() == 1 {
//             to_return.extend(to_sort.remove(0));
//         } else {
//             for (idx, it) in to_sort.iter_mut().enumerate() {
//                 let curr_packet = it.peek();
//                 if let Some(curr_packet) = curr_packet {
//                     let curr_ts = *curr_packet.timestamp();
//                     current_lowest = current_lowest.map(|(prev_idx, prev)| {
//                         match curr_ts.cmp(&prev) {
//                             Ordering::Less => {
//                                 (idx, curr_ts)
//                             },
//                             _ => {
//                                 (prev_idx, prev)
//                             }
//                         }
//                     }).or_else(|| Some((idx, curr_ts)));
//                 }
//             }
//         }
//
//         to_sort = to_sort.into_iter().filter_map(|mut p| {
//             if p.peek().is_some() {
//                 Some(p)
//             } else {
//                 None
//             }
//         }).collect();
//
//         if let Some((idx, _)) = current_lowest {
//             let packet_opt = to_sort
//                 .get_mut(idx)
//                 .iter_mut()
//                 .flat_map(|it| it.next())
//                 .next();
//             if let Some(packet) = packet_opt {
//                 to_return.push(packet)
//             }
//         } else {
//             break;
//         }
//     }
//     to_return
// }

fn gather_packets<E: Fail + Sync + Send, T: Stream<Item = StreamItem<E>> + Sized + Unpin>(
    stream_states: &mut VecDeque<BridgeStreamState<E, T>>,
) -> Vec<Packet> {
    let mut result = vec![];
    let mut gather_to: Option<SystemTime> = None;

    for s in stream_states.iter() {
        let last_time = s
            .current
            .last()
            .iter()
            .flat_map(|p| p.last())
            .last()
            .map(|p| *p.timestamp());

        if let Some(last_time) = last_time {
            gather_to = gather_to
                .map(|prev| prev.min(last_time))
                .or(Some(last_time));
        }
    }

    if let Some(gather_to) = gather_to {
        for s in stream_states.iter_mut() {
            let current = std::mem::take(&mut s.current);
            let (to_send, to_keep) = current
                .into_iter()
                .flat_map(|ps| ps.into_iter())
                .partition(|p| p.timestamp() <= &gather_to);

            let to_keep: Vec<Packet> = to_keep;
            if !to_keep.is_empty() {
                s.current.push(to_keep);
            }
            result.extend(to_send)
        }
    } else {
    }
    result.sort_by_key(|p| *p.timestamp()); // todo convert
    result
}

impl<E: Fail + Sync + Send, T: Stream<Item = StreamItem<E>> + Sized + Unpin> Stream
    for BridgeStream<E, T>
{
    type Item = StreamItem<E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        //trace!("Interfaces: {:?}", this.stream_states.len());
        let states: &mut VecDeque<BridgeStreamState<E, T>> = this.stream_states;
        let max_buffer_time = this.max_buffer_time;
        let mut max_time_spread: Duration = Duration::from_millis(0);
        let mut delay_count = 0;
        for state in states.iter_mut() {
            max_time_spread = state.spread().max(max_time_spread);
            match Pin::new(&mut state.stream).poll_next(cx) {
                Poll::Pending => {
                    trace!("Return Pending");
                    delay_count = delay_count + 1;
                    continue;
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    trace!("Interface has completed");
                    state.complete = true;
                    continue;
                }
                Poll::Ready(Some(Ok(v))) => {
                    trace!("Poll returns with {} packets", v.len());
                    if v.is_empty() {
                        trace!("Poll returns with no packets");
                        delay_count = delay_count + 1;
                        continue;
                    }
                    state.current.push(v);
                }
            }
        }

        let one_buffer_is_over = max_time_spread > *max_buffer_time;

        let ready_count = states
            .iter()
            .filter(|s| s.current.len() >= 2 || s.complete)
            .count();

        let res = if ready_count == states.len() || one_buffer_is_over {
            gather_packets(states)
        } else {
            trace!("Not reporting");
            vec![]
        };

        states.retain(|iface| {
            //drop the complete interfaces
            return !iface.is_complete();
        });

        if res.is_empty() && states.is_empty() {
            trace!("All ifaces are complete.");
            return Poll::Ready(None);
        } else if delay_count >= states.len() && !states.is_empty() {
            trace!("All ifaces are delayed.");
            return Poll::Pending;
        } else {
            return Poll::Ready(Some(Ok(res)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PacketStream;
    use byteorder::{ByteOrder, ReadBytesExt};
    use failure::_core::time::Duration;
    use futures::stream;
    use futures::{Future, Stream};
    use rand;
    use std::io::Cursor;
    use std::ops::Range;
    use std::path::PathBuf;

    fn make_packet(ts: usize) -> Packet {
        Packet {
            timestamp: SystemTime::UNIX_EPOCH + Duration::from_millis(ts as _),
            actual_length: 0,
            original_length: 0,
            data: vec![],
        }
    }
    /*
    #[test]
    fn sort_correctly() {
        let max = 5000;
        let to_sort1: Vec<Packet>  = {
            let mut r = (0..max)
                .map(|_| rand::random())
                .collect::<Vec<usize>>();
            r.sort();
            r.into_iter().map(|i|{make_packet(i as _)})
                .collect::<Vec<Packet>>()
        };
        let to_sort2: Vec<Packet>  = {
            let mut r = (0..max)
                .map(|_| rand::random())
                .collect::<Vec<usize>>();
            r.sort();
            r.into_iter().map(|i|{make_packet(i as _)})
                .collect::<Vec<Packet>>()
        };
        let to_sort3: Vec<Packet>  = {
            let mut r = (0..max)
                .map(|_| rand::random())
                .collect::<Vec<usize>>();
            r.sort();
            r.into_iter().map(|i|{make_packet(i as _)})
                .collect::<Vec<Packet>>()
        };

        let start_ts = SystemTime::now();
        let mut acc = vec![to_sort1.clone(), to_sort2.clone(), to_sort3.clone()].into_iter().flatten().collect::<Vec<Packet>>();
        acc.sort_by_key(|p| p.timestamp);
        let taken = start_ts.elapsed().unwrap();
        println!("Normal sort time: {:?}", taken);

        let len = to_sort1.len() + to_sort2.len() + to_sort1.len();
        let to_sort = vec![to_sort1.into_iter().peekable(), to_sort2.into_iter().peekable(), to_sort3.into_iter().peekable()];
        let start_ts = SystemTime::now();
        let sorted = sort_packets(to_sort, len);
        let taken = start_ts.elapsed().unwrap();
        println!("PAcket sort time: {:?}", taken);
        let sorted = sorted.into_iter().map(|p| p.timestamp).collect::<Vec<_>>();
        let acc = acc.into_iter().map(|p| p.timestamp).collect::<Vec<_>>();
        assert_eq!(sorted, acc);
    }*/

    #[tokio::test]
    async fn packets_from_file() {
        let _ = env_logger::try_init();

        let pcap_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("canary.pcap");

        info!("Testing against {:?}", pcap_path);

        let handle = Handle::file_capture(pcap_path.to_str().expect("No path found"))
            .expect("No handle created");

        let packet_stream =
            PacketStream::new(Config::default(), Arc::clone(&handle)).expect("Failed to build");

        let packet_provider = BridgeStream::new(vec![packet_stream], Duration::from_millis(100))
            .expect("Failed to build");

        let fut_packets = packet_provider.collect::<Vec<_>>();
        let packets: Vec<_> = fut_packets
            .await
            .into_iter()
            .flatten()
            .flatten()
            .filter(|p| p.data().len() == p.actual_length() as usize)
            .collect();

        handle.interrupt();

        assert_eq!(packets.len(), 10);

        let packet = packets.first().cloned().expect("No packets");
        let data = packet
            .into_pcap_record::<byteorder::BigEndian>()
            .expect("Failed to convert to pcap record");
        let mut cursor = Cursor::new(data);
        let ts_sec = cursor
            .read_u32::<byteorder::BigEndian>()
            .expect("Failed to read");
        let ts_usec = cursor
            .read_u32::<byteorder::BigEndian>()
            .expect("Failed to read");
        let actual_length = cursor
            .read_u32::<byteorder::BigEndian>()
            .expect("Failed to read");
        assert_eq!(
            ts_sec as u64 * 1_000_000 as u64 + ts_usec as u64,
            1513735120021685
        );
        assert_eq!(actual_length, 54);
    }
    #[tokio::test]
    async fn packets_from_file_next_bridge() {
        let _ = env_logger::try_init();

        let pcap_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("canary.pcap");

        info!("Testing against {:?}", pcap_path);

        let handle = Handle::file_capture(pcap_path.to_str().expect("No path found"))
            .expect("No handle created");

        let packet_stream =
            PacketStream::new(Config::default(), Arc::clone(&handle)).expect("Failed to build");

        let packet_provider = BridgeStream::new(vec![packet_stream], Duration::from_millis(100))
            .expect("Failed to build");

        let fut_packets = async move {
            let mut packet_provider = packet_provider.boxed();
            let mut packets = vec![];
            while let Some(p) = packet_provider.next().await {
                info!("packets returned {:?}", p);
                packets.extend(p);
            }
            packets
        };
        let packets = fut_packets
            .await
            .into_iter()
            .flatten()
            .filter(|p| p.data().len() == p.actual_length() as _)
            .count();

        handle.interrupt();

        assert_eq!(packets, 10);
    }

    #[test]
    fn packets_from_lookup_bridge() {
        let _ = env_logger::try_init();

        let handle = Handle::lookup().expect("No handle created");
        let packet_stream =
            PacketStream::new(Config::default(), Arc::clone(&handle)).expect("Failed to build");

        let stream = BridgeStream::new(vec![packet_stream], Duration::from_millis(100));

        assert!(
            stream.is_ok(),
            format!("Could not build stream {}", stream.err().unwrap())
        );
    }

    #[test]
    fn packets_from_lookup_with_bpf() {
        let _ = env_logger::try_init();

        let mut cfg = Config::default();
        cfg.with_bpf(
            "(not (net 172.16.0.0/16 and port 443)) and (not (host 172.17.76.33 and port 443))"
                .to_owned(),
        );
        let handle = Handle::lookup().expect("No handle created");
        let packet_stream =
            PacketStream::new(Config::default(), Arc::clone(&handle)).expect("Failed to build");

        let stream = BridgeStream::new(vec![packet_stream], Duration::from_millis(100));

        assert!(
            stream.is_ok(),
            format!("Could not build stream {}", stream.err().unwrap())
        );
    }
    #[tokio::test]
    async fn packets_come_out_time_ordered() {
        let mut packets1 = vec![];
        let mut packets2 = vec![];

        let base_time = std::time::SystemTime::UNIX_EPOCH;

        for s in 0..20 {
            let d = base_time + std::time::Duration::from_secs(s);
            let p = Packet::new(d, 0, 0, vec![]);
            packets1.push(p)
        }

        for s in 5..15 {
            let d = base_time + std::time::Duration::from_secs(s);
            let p = Packet::new(d, 0, 0, vec![]);
            packets2.push(p)
        }

        let item1: StreamItem<Error> = Ok(packets1.clone());
        let item2: StreamItem<Error> = Ok(packets2.clone());

        let stream1 = futures::stream::iter(vec![item1]);
        let stream2 = futures::stream::iter(vec![item2]);

        let bridge = BridgeStream::new(vec![stream1, stream2], Duration::from_millis(100));

        let mut result = bridge
            .expect("Unable to create BridgeStream")
            .collect::<Vec<StreamItem<Error>>>()
            .await;
        let result = result
            .into_iter()
            .map(|r| r.unwrap())
            .flatten()
            .collect::<Vec<Packet>>();
        info!("Result {:?}", result);

        let mut expected = vec![packets1, packets2]
            .into_iter()
            .flatten()
            .collect::<Vec<Packet>>();
        expected.sort_by_key(|p| p.timestamp().clone());
        let expected_time = expected.iter().map(|p| p.timestamp()).collect::<Vec<_>>();
        let result_time = result.iter().map(|p| p.timestamp()).collect::<Vec<_>>();
        assert_eq!(result.len(), expected.len());
        assert_eq!(result_time, expected_time);

        info!("result: {:?}", result);
        info!("expected: {:?}", expected);
    }
}
