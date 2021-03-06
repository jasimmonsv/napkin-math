#[macro_use]
extern crate byte_unit;
extern crate clap;
extern crate regex;

// use std::alloc::System;
// #[global_allocator]
// static A: System = System;

extern crate libc;
use libc::{c_int, c_long, madvise, size_t, timeval};

#[cfg(target_os = "linux")]
use libc::posix_fadvise;

use regex::Regex;

#[cfg(target_os = "linux")]
use rio::{Rio, Uring};

extern crate jemallocator;
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

// https://ark.intel.com/content/www/us/en/ark/products/97185/intel-core-i7-7700hq-processor-6m-cache-up-to-3-80-ghz.html
// https://en.wikichip.org/wiki/intel/core_i7/i7-7700hq
//
// Single: 17.88 GiB/s
// Dual:   37.5 GB/s
//
// L1: 32 KiB
// L2: 262 KiB
// L3: 6 MiB
//
// sysctl -a | grep cache <---
//
// TODO: Would be cool to instrument branch misses etc. here
use byte_unit::Byte;
use clap::{App, Arg, SubCommand};
use failure::Error;
use num_format::{Locale, ToFormattedString};
use page_size;
use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use rand::{thread_rng, Rng, SeedableRng};
use redis::{Client, Commands};
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::prelude::*;
use std::io::SeekFrom;
use std::mem::forget;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

#[allow(unused_imports)]
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::__rdtscp;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

// TODO: use this instead
// from bencher::black_box, avoid compiler dead-code optimizations.
pub fn black_box<T>(dummy: T) -> T {
    unsafe {
        let ret = ptr::read_volatile(&dummy);
        forget(dummy);
        ret
    }
}

// TODO: Probably we should just expose duration and iterations, and correct for duration_ratio
// directly in whatever produces this data structure to simplify consumption.
struct BenchmarkResult {
    iterations: usize,
    duration: Duration,
    // duration_ratio: f64,
    // intended_duration: Duration,
    cycles: u64,
}

impl BenchmarkResult {
    fn print_results(&self, name: &str, size_of_type: usize) {
        let mut name = String::from(name);
        if size_of_type > 0 {
            name.push_str(&format!(
                " <{}>",
                Byte::from_bytes(size_of_type as u128)
                    .get_appropriate_unit(true)
                    .format(0)
            ));
        }

        println!(
            "\n[{}] Iterations in {} miliseconds, no overhead: {}",
            name,
            self.duration.as_millis(),
            self.iterations.to_formatted_string(&Locale::en)
        );

        println!(
            "[{}] Iterations / second: {}",
            name,
            ((((self.iterations as f64 / self.duration.as_millis() as f64) as f64) * 1000.0)
                as u128)
                .to_formatted_string(&Locale::en)
        );

        if size_of_type > 0 {
            println!(
                "[{}] Bytes handled per iteration: {} bytes",
                name, size_of_type
            );

            let total_bytes_pushed = size_of_type * self.iterations;
            println!(
                "[{}] Total bytes processed: {}",
                name,
                Byte::from_bytes(total_bytes_pushed as u128)
                    .get_appropriate_unit(true)
                    .format(3)
            );

            let bytes_per_second =
                ((total_bytes_pushed as f64) / self.duration.as_millis() as f64) * 1000.0;
            println!(
                "[{}] Throughput: {}/s",
                name,
                // TODO: Too hard to get right when values aren't just printed!
                Byte::from_bytes(bytes_per_second as u128)
                    .get_appropriate_unit(true)
                    .format(3)
            );
        }

        // TODO handle less than 1ns
        let single_operation_nanoseconds =
            Duration::from_nanos(self.duration.as_nanos() as u64 / self.iterations as u64);

        let time_unit = if single_operation_nanoseconds.as_nanos() <= 10 {
            format!(
                "{:.3} ns",
                self.duration.as_nanos() as f64 / self.iterations as f64
            )
        } else {
            self.get_appropriate_time_unit(single_operation_nanoseconds)
        };

        println!("[{}] Avg single iteration: {}", name, time_unit);

        let single_operation_cycles = self.cycles as f64 / (self.iterations as f64);

        println!(
            "[{}] Avg single iteration cycles: {:.2}",
            name, single_operation_cycles,
        );

        if size_of_type > 0 {
            let single_op_nanos = self.duration.as_nanos() as f64 / self.iterations as f64;
            let nanoseconds_per_byte = 1.0 / ((size_of_type as f64) / single_op_nanos);
            let nanoseconds_per_mebibyte = nanoseconds_per_byte * n_mib_bytes!(1) as f64;
            let duration_per_mebibyte = Duration::from_nanos(nanoseconds_per_mebibyte as u64);

            println!(
                "[{}] Time to process 1 MiB: {}",
                name,
                self.get_appropriate_time_unit(duration_per_mebibyte),
            );

            let nanoseconds_per_gibibyte = nanoseconds_per_byte * n_gib_bytes!(1) as f64;
            let duration_per_gibibyte = Duration::from_nanos(nanoseconds_per_gibibyte as u64);

            println!(
                "[{}] Time to process 1 GiB: {}",
                name,
                self.get_appropriate_time_unit(duration_per_gibibyte),
            );

            let nanoseconds_per_tibibyte = nanoseconds_per_byte * n_tib_bytes!(1) as f64;
            let duration_per_tibibyte = Duration::from_nanos(nanoseconds_per_tibibyte as u64);

            println!(
                "[{}] Time to process 1 TiB: {}",
                name,
                self.get_appropriate_time_unit(duration_per_tibibyte),
            );
        }
    }

    // impl on duration
    fn get_appropriate_time_unit(&self, duration: Duration) -> String {
        if duration.as_nanos() < 1000 {
            format!("{} ns", duration.as_nanos())
        } else if duration.as_nanos() > 1000 && duration.as_millis() < 5 {
            format!("{} μs", duration.as_micros())
        } else if duration.as_micros() > 1000 && duration.as_millis() < 3000 {
            format!("{} ms", duration.as_millis())
        } else if duration.as_secs() <= 120 {
            format!("{:.2} s", duration.as_millis() as f64 / 1000.0)
        } else if duration.as_secs() <= 3600 {
            format!("{:.2} min", (duration.as_secs() as f64) / 60.0)
        } else {
            format!("{:.2} hours", (duration.as_secs() as f64) / 3600.0)
        }
    }
}

fn benchmark<T, F: Fn() -> (T), V: FnMut(&mut T) -> bool>(
    setup: F,
    mut f: V,
) -> Result<BenchmarkResult, Error> {
    // warmup run
    let mut val = setup();
    let intended_duration = Duration::from_millis(100);
    let mut iterations_per_check = 1;
    let mut iterations: usize = 0;
    let instant = Instant::now();

    // The reason for the "done" and boolean return type here is that some benchmarks may want to
    // finish earlier, e.g. random disk reads want to finish as soon as it's read every page since
    // otherwise we're just benchmarking memory. If this is the only use-case, maybe we should just
    // make sure this never happens.
    let mut done = false;
    while instant.elapsed() < intended_duration {
        for i in 1..(iterations_per_check + 1) {
            if !f(&mut val) {
                done = true;
                iterations_per_check = i;
                break;
            }
        }
        iterations += iterations_per_check;
        if done {
            break;
        }
    }

    // real run
    let mut val = setup();
    let rdtsc_before: u64;
    let intended_duration = Duration::from_millis(5000);
    let mut iterations_per_check = iterations;
    let mut iterations: usize = 0;
    let instant = Instant::now();
    unsafe {
        rdtsc_before = core::arch::x86_64::_rdtsc();
    }

    let mut done = false;
    while instant.elapsed() < intended_duration {
        for i in 1..(iterations_per_check + 1) {
            // unlikely branch
            if !f(&mut val) {
                done = true;
                iterations_per_check = i;
                break;
            }
        }
        iterations += iterations_per_check;
        if done {
            break;
        }
    }
    let actual_duration = instant.elapsed();

    let rdtsc_after: u64;
    unsafe {
        rdtsc_after = core::arch::x86_64::_rdtsc();
    }

    Ok(BenchmarkResult {
        iterations: iterations,
        duration: actual_duration,
        // duration_ratio: intended_duration.as_nanos() as f64 / actual_duration.as_nanos() as f64,
        // intended_duration: intended_duration,
        cycles: rdtsc_after - rdtsc_before,
    })
}

// TODO: take args for how long to perform tests
fn main() {
    let matches = App::new("Napkin Math")
        .version("0.1")
        .author("Simon Eskildsen <simon@sirupsen.com>")
        .about("Runs computing benchmarks to find numbers for napkin math.")
        .arg(
            Arg::with_name("evaluate")
                .long("evaluate")
                .short("e")
                .help("Run tests that match a regex")
                .value_name("REGEX")
                .takes_value(true),
        )
        .get_matches();

    let methods: [(&'static str, fn()); 17] = [
        ("memory_read_sequential", memory_read_sequential),
        ("memory_write_sequential", memory_write_sequential),
        ("memory_read_random", memory_read_random),
        ("memory_write_random", memory_write_random),
        ("syscall_getpid", syscall_getpid),
        ("syscall_time", syscall_time),
        ("syscall_getrusage", syscall_getrusage),
        ("syscall_stat", syscall_stat),
        ("disk_read_sequential", disk_read_sequential),
        ("disk_read_random", disk_read_random),
        (
            "disk_write_sequential_no_fsync",
            disk_write_sequential_no_fsync,
        ),
        (
            "disk_read_sequential_io_uring",
            disk_read_sequential_io_uring,
        ),
        ("disk_write_sequential_fsync", disk_write_sequential_fsync),
        ("tcp_read_write", tcp_read_write),
        ("simd", simd),
        ("redis_read_single_key", redis_read_single_key),
        ("sort", sort),
    ];

    if matches.occurrences_of("evaluate") > 0 {
        let regex_argument = matches.value_of("evaluate").unwrap_or(".*");
        println!("Matching tests with regex: {}", regex_argument);
        let regex = Regex::new(regex_argument).unwrap();

        for (name, func) in &methods {
            if regex.is_match(name) {
                println!("\nExecuting {}..", name);
                func();
            }
        }
    }
}

fn memory_write_sequential() {
    struct Test {
        i: usize,
        vec: Vec<[u64; 8]>,
    }
    let bytes_per_iteration = 64;
    let size_in_elements = (n_gb_bytes!(1) as u64 / bytes_per_iteration) as usize;

    let result = benchmark(
        || {
            let mut vec = Vec::new();
            vec.resize(size_in_elements, [1, 2, 3, 4, 5, 6, 7, 8]);
            Test { i: 0, vec }
        },
        |test| {
            test.vec[test.i] = [8, 7, 110694, 5, 4, 3, 2, 1];
            black_box(test.vec[test.i]);
            test.i += 1;
            if test.i == test.vec.len() {
                return false;
            }
            return true;
        },
    )
    .unwrap();

    result.print_results("Write Seq Vec", 64);
}

fn memory_read_sequential() {
    let bytes_per_iteration = 64;
    let size_in_elements = (n_gb_bytes!(1) as u64 / bytes_per_iteration) as u64;

    struct Test {
        i: usize,
        vec: Vec<[u64; 8]>,
    }

    // put these in separate functions so they can be disassembled.
    // #[inline] is going to be important here.
    let result = benchmark(
        || {
            let mut vec: Vec<[u64; 8]> = Vec::new();
            for i in 0..size_in_elements {
                vec.push([i, i, i, i, i, i, i, i])
            }

            Test { i: 0, vec }
        },
        |test| {
            black_box(test.vec[test.i]);
            test.i += 1;
            if test.i == test.vec.len() {
                return false;
            }

            true
        },
    )
    .unwrap();

    result.print_results("Read Seq Vec", bytes_per_iteration as usize);
}

fn memory_write_random() {
    struct Test {
        vec: Vec<[u64; 8]>,
        order: Vec<usize>,
        i: usize,
    }

    let bytes_per_iteration = 64;
    let size_in_elements = (n_gb_bytes!(1) as u64 / bytes_per_iteration) as usize;

    let result = benchmark(
        || {
            let mut vec = Vec::new();
            vec.resize(size_in_elements, [1, 2, 3, 4, 5, 6, 7, 8]);

            let mut order: Vec<usize> = (0..size_in_elements).collect();
            order.shuffle(&mut thread_rng());
            Test { vec, order, i: 0 }
        },
        |test| {
            test.vec[test.order[test.i]] = [8, 7, 6, 5, 4, 3, 2, 1];
            black_box(test.vec[test.order[test.i]]);
            test.i += 1;
            if test.i == test.vec.len() {
                return false;
            }
            true
        },
    )
    .unwrap();

    result.print_results("Random Write Vec", bytes_per_iteration as usize);
}

struct MemoryReadTest {
    vec: Vec<[u64; 8]>,
    order: Vec<usize>,
    i: usize,
}

fn memory_read_random() {
    let result = benchmark(memory_read_random_setup, memory_read_random_iteration).unwrap();
    result.print_results("Random Read Vec", 64 as usize);
}

fn memory_read_random_setup() -> MemoryReadTest {
    let size_in_elements = (n_gb_bytes!(1) as u64 / 64) as usize;

    let mut vec = Vec::new();
    vec.resize(size_in_elements, [1, 2, 3, 4, 5, 6, 7, 8]);
    unsafe {
        let data = vec.as_mut_ptr() as *mut libc::c_void;
        libc::madvise(data, size_in_elements, libc::MADV_RANDOM);
    }

    let mut order: Vec<usize> = (0..size_in_elements).collect();
    unsafe {
        let data = order.as_mut_ptr() as *mut libc::c_void;
        libc::madvise(data, size_in_elements, libc::MADV_SEQUENTIAL);
    }
    order.shuffle(&mut thread_rng());
    MemoryReadTest { vec, order, i: 0 }
}

#[inline(always)]
fn memory_read_random_iteration(test: &mut MemoryReadTest) -> bool {
    black_box(test.vec[test.order[test.i]]);
    test.i += 1;
    if test.i == test.vec.len() {
        return false;
    }
    true
}

fn disk_write_sequential_fsync() {
    struct Test {
        bytes: Vec<u8>,
        file: std::fs::File,
    }

    let file_name = "/tmp/napkin.txt";
    let size_of_writes = n_kib_bytes!(8) as usize;

    let result = benchmark(
        || {
            let file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(file_name)
                .unwrap();

            let bytes: Vec<u8> = (0..size_of_writes).map(|_| rand::random::<u8>()).collect();

            Test { bytes, file }
        },
        |test| {
            test.file.write_all(&test.bytes).unwrap();
            test.file.sync_data().unwrap();
            true
        },
    )
    .unwrap();
    fs::remove_file(file_name);

    result.print_results("Sequential Disk Write, Fsync", size_of_writes);
}

fn disk_write_sequential_no_fsync() {
    struct Test {
        bytes: Vec<u8>,
        file: std::fs::File,
    }

    let file_name = "/tmp/napkin.txt";
    let size_of_writes = n_kib_bytes!(8) as usize;

    let result = benchmark(
        || {
            let file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(file_name)
                .unwrap();

            let bytes: Vec<u8> = (0..size_of_writes).map(|_| rand::random::<u8>()).collect();

            Test { bytes, file }
        },
        |test| {
            test.file.write_all(&test.bytes).unwrap();
            true
        },
    )
    .unwrap();
    fs::remove_file(file_name);

    result.print_results("Sequential Disk Write, No Fsync", size_of_writes);
}

fn disk_read_sequential() {
    const BUF_SIZE: usize = n_kib_bytes!(64) as usize;

    struct Test {
        buffer: [u8; BUF_SIZE],
        file: fs::File,
    }
    let file_name = "/tmp/napkin.txt";

    let result = benchmark(
        || {
            // flush page cache? prob not necessary since we re-create the file.
            fs::remove_file(file_name);
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .read(true)
                .open(file_name)
                .unwrap();
            let buffer = vec![0; n_gib_bytes!(1) as usize];
            file.write_all(&buffer).unwrap();
            file.sync_data().unwrap();

            let buffer: [u8; BUF_SIZE] = [0; BUF_SIZE];
            file.seek(SeekFrom::Start(0)).unwrap();

            unsafe {
                #[cfg(target_os = "linux")]
                libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL);
            }

            Test { buffer, file }
        },
        |test| {
            let n = test.file.read(&mut test.buffer).unwrap();
            // TODO: this is cheating...
            if n < BUF_SIZE {
                test.file.seek(SeekFrom::Start(0)).unwrap();
            };
            true
        },
    )
    .unwrap();
    fs::remove_file(file_name);

    result.print_results("Sequential Disk Read", BUF_SIZE);
}

#[cfg(target_os = "macos")]
fn disk_read_sequential_io_uring() {
    println!("only supported on linux");
}

#[cfg(target_os = "linux")]
fn disk_read_sequential_io_uring() {
    // https://github.com/axboe/liburing/blob/master/examples/io_uring-cp.c
    const BUF_SIZE: usize = n_kib_bytes!(32) as usize;
    let reads_per_iteration: isize = 64;

    struct Test {
        buffers: Vec<Vec<u8>>,
        file: fs::File,
        ring: rio::Rio,
        size: usize,
        offset: usize,
    }
    let file_name = "/tmp/napkin.txt";
    use std::slice;

    // TODO: checksum somehow

    let result = benchmark(
        || {
            // flush page cache? prob not necessary since we re-create the file.
            fs::remove_file(file_name);
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .read(true)
                .open(file_name)
                .unwrap();
            let buffer = vec![0; n_gib_bytes!(1) as usize];
            file.write_all(&buffer).unwrap();
            file.sync_data().unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();

            // flush page cache after this

            unsafe {
                #[cfg(target_os = "linux")]
                libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL);
            }

            let mut ring = rio::new().expect("create uring");
            let mut buffers = vec![vec![0; BUF_SIZE]; reads_per_iteration as usize];
            Test { buffers, file, ring, size: n_gib_bytes!(1) as usize, offset: 0 }
        },
        |test| {
            let ptr = test.buffers.as_mut_ptr();
            let mut completions = vec![];

            for i in 0..reads_per_iteration {
                if test.size <= 0 {
                    println!("Stopping early");
                    break
                }

                unsafe {
                    let buf = &slice::from_raw_parts_mut(ptr.offset(i), 1)[0];
                    completions.push(test.ring.read_at(&test.file, buf, test.offset as u64));
                }

                test.offset += BUF_SIZE;
                test.size -= BUF_SIZE;
            }

            for completion in completions.into_iter() {
                let read = completion.wait().unwrap();
                if read < BUF_SIZE {
                    println!("at end?");
                }
            }

            if test.size <= 0 {
                test.offset = 0;
                test.size = n_gib_bytes!(1) as usize;
            }

            true
        },
    )
    .unwrap();
    fs::remove_file(file_name);

    result.print_results("Io-uring Sequential Disk Read", BUF_SIZE * (reads_per_iteration as usize));
}

fn disk_read_random() {
    const BUF_SIZE: usize = n_kib_bytes!(8) as usize;

    struct Test {
        rng: SmallRng,
        file_length: u64,
        buffer: [u8; BUF_SIZE],
        pages: Vec<u64>,
        i: usize,
        file: std::fs::File,
    }
    let file_name = "/tmp/napkin.txt";
    let page_size = page_size::get();

    let result = benchmark(
        || {
            fs::remove_file(file_name);
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .read(true)
                .open(file_name)
                .unwrap();
            let buffer = vec![0; n_gib_bytes!(8) as usize];
            file.write_all(&buffer).unwrap();
            file.sync_data().unwrap();

            // This is to ensure we only visit each page once. Otherwise this is essentially just
            // benchmarking syscall + page cache, which is going to be awfully close to random
            // memory read.
            let mut pages: Vec<u64> = Vec::new();
            for i in 0..(buffer.len() / page_size) {
                pages.push((i * page_size + 1) as u64);
            }
            pages.shuffle(&mut thread_rng());

            unsafe {
                #[cfg(target_os = "linux")]
                libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_RANDOM);
            }

            let buffer: [u8; BUF_SIZE] = [0; BUF_SIZE];
            let metadata = fs::metadata(file_name).unwrap();

            Test {
                file,
                file_length: metadata.len() - buffer.len() as u64,
                rng: SmallRng::from_entropy(),
                pages,
                buffer,
                i: 0,
            }
        },
        |test| {
            test.file.seek(SeekFrom::Start(test.pages[test.i])).unwrap();
            test.file.read_exact(&mut test.buffer).unwrap();
            black_box(test.buffer);
            test.i += 1;

            if test.i == test.pages.len() {
                return false;
            };

            true
        },
    )
    .unwrap();
    fs::remove_file(file_name);

    result.print_results("Random Disk Seek, No Page Cache", BUF_SIZE);
}

// this comes from the auxilirary vector on some OSes, making this not do a syscall.
// on the linux kernel I've been testing on, it does do a syscall. on darwin, it doesn't.
fn syscall_getpid() {
    use std::process;

    let result = benchmark(
        || {},
        |test| {
            black_box(process::id());
            true
        },
    )
    .unwrap();
    result.print_results("Sycall getpid(2)", 0);
}

// this is available in user-space memory (depending on libc) and often doesn't result in a sycall.
fn syscall_time() {
    let result = benchmark(
        || {},
        |test| {
            black_box(SystemTime::now());
            true
        },
    )
    .unwrap();
    result.print_results("Sycall gettimeofday(2)", 0);
}

// syscall, can't be optimized out
fn syscall_getrusage() {
    let time = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut rusage = Box::new(libc::rusage {
        ru_utime: time.clone(),
        ru_stime: time.clone(),
        ru_maxrss: 0,
        ru_ixrss: 0,
        ru_idrss: 0,
        ru_isrss: 0,
        ru_minflt: 0,
        ru_majflt: 0,
        ru_nswap: 0,
        ru_inblock: 0,
        ru_oublock: 0,
        ru_msgsnd: 0,
        ru_msgrcv: 0,
        ru_nsignals: 0,
        ru_nvcsw: 0,
        ru_nivcsw: 0,
    });
    let ptr = Box::into_raw(rusage);

    let result = benchmark(
        || {},
        |test| {
            unsafe {
                libc::getrusage(0, ptr);
            }
            true
        },
    )
    .unwrap();
    result.print_results("Sycall getrusage(2)", 0);
}

// syscall, can't be optimized out
fn syscall_stat() {
    let mut f = fs::File::open("/tmp").unwrap();

    let result = benchmark(
        || {},
        |test| {
            let metadata = f.metadata().unwrap();
            black_box(metadata);
            true
        },
    )
    .unwrap();
    result.print_results("Sycall stat(2)", 0);
}

fn tcp_read_write() {
    // This server doesn't support multiple clients.
    thread::spawn(move || {
        let listener = TcpListener::bind("127.0.0.1:8877").unwrap();
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();

            stream.set_nodelay(true).unwrap();
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(1000)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_millis(1000)))
                .unwrap();

            let mut buffer: [u8; 64] = [0; 64];
            let mut i = 0;

            loop {
                match stream.read(&mut buffer) {
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // println!("s{}: failed to read, err: {:?}..", i, e);
                        continue;
                    }
                    Ok(n) => {
                        // println!("s{}: read: {}", i, n);

                        match stream.write(&buffer[..n]) {
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                // println!("s{}: failed to write", i);
                                continue;
                            }
                            Ok(n) => {
                                // println!("s{}: write: {}", i, n);
                            }
                            Err(e) => panic!(e),
                        };
                    }
                    Err(e) => panic!(e),
                };

                // i += 1;
            }
        }
    });

    let bytes: Vec<u8> = (0..64).map(|_| rand::random::<u8>()).collect();
    let mut buffer: [u8; 64] = [0; 64];

    // This is done outside the setup block to avoid having to deal with a shutdown signal..
    let mut stream = TcpStream::connect("127.0.0.1:8877").unwrap();
    stream.set_nodelay(true).unwrap();
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(1000)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(1000)))
        .unwrap();

    let result = benchmark(
        || {},
        |_| {
            match stream.write(&bytes) {
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // println!("c: failed to write");
                    return true;
                }
                Ok(n) => {
                    // println!("c: write: {}", n);

                    match stream.read(&mut buffer[0..n]) {
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            // println!("c: failed to read, err: {:?}..", e);
                            return true;
                        }
                        Ok(_n) => {
                            // println!("c: read: {}\n", n);
                        }
                        Err(e) => {
                            // println!("omgs read! {:?}", e.raw_os_error());
                            panic!(e)
                        }
                    };
                }
                Err(e) => {
                    // println!("omgs write! {:?}", e.raw_os_error());
                    panic!(e)
                }
            };

            true
        },
    )
    .unwrap();

    result.print_results("Tcp Echo", 64);
}

#[derive(Clone, Copy)]
#[allow(non_camel_case_types)]
pub union i32simd {
    vector: __m256i,
    numbers: [u32; 8],
}

fn simd() {
    unsafe {
        let a = i32simd {
            vector: _mm256_set_epi32(1, 2, 3, 4, 5, 6, 7, 8),
        };
        let b = i32simd {
            vector: _mm256_set_epi32(1, 2, 3, 4, 5, 6, 7, 8),
        };
        let result = i32simd {
            vector: _mm256_mul_epi32(a.vector, b.vector),
        };
        let result2 = i32simd {
            vector: _mm256_mullo_epi32(a.vector, b.vector),
        };
        println!("{:?}", result.numbers);
        println!("{:?}", result2.numbers);
    }
}

fn redis_read_single_key() {
    let client = redis::Client::open("redis://127.0.0.1/").unwrap();

    let result = benchmark(
        || {
            let mut con = client.get_connection().unwrap();
            let bytes: Vec<u8> = (0..64).map(|_| rand::random::<u8>()).collect();
            let _: () = con.set("1", bytes).unwrap();
            con
        },
        |con| {
            let _: Vec<u8> = con.get("1").unwrap();
            true
        },
    )
    .unwrap();

    result.print_results("Redis Read", 64);
}

fn sort() {
    const TOTAL_SIZE: usize = n_mib_bytes!(1) as usize;

    let result = benchmark(
        || {
            let elements = TOTAL_SIZE / 8;
            let mut bytes: Vec<u64> = (0..elements).map(|_| rand::random::<u64>()).collect();
            bytes
        },
        |mut bytes| {
            bytes.sort_unstable();
            // TODO: enum to re-start with setup or stop entirely
            false
        },
    )
    .unwrap();

    result.print_results("Sort", TOTAL_SIZE);
}
