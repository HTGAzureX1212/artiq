#![feature(lang_items, asm, panic_unwind, libc,
           panic_info_message, nll, c_unwind)]
#![no_std]

extern crate byteorder;
extern crate libc;
extern crate unwind;
extern crate cslice;

extern crate eh;
extern crate io;
extern crate dyld;
extern crate board_misoc;
extern crate board_artiq;
extern crate proto_artiq;
extern crate riscv;

use core::{mem, ptr, slice, str, convert::TryFrom};
use cslice::CSlice;
use io::Cursor;
use dyld::Library;
use board_artiq::{mailbox, rpc_queue};
use proto_artiq::{kernel_proto, rpc_proto};
use kernel_proto::*;
use board_misoc::csr;
use riscv::register::{mcause, mepc, mtval};

fn send(request: &Message) {
    unsafe { mailbox::send(request as *const _ as usize) }
    while !mailbox::acknowledged() {}
}

fn recv<R, F: FnOnce(&Message) -> R>(f: F) -> R {
    let mut msg_ptr = 0;
    while msg_ptr == 0 { msg_ptr = mailbox::receive(); }
    let result = f(unsafe { &*(msg_ptr as *const Message) });
    mailbox::acknowledge();
    result
}

fn try_recv<F: FnOnce(&Message)>(f: F) {
    let msg_ptr = mailbox::receive();
    if msg_ptr != 0 {
        f(unsafe { &*(msg_ptr as *const Message) });
        mailbox::acknowledge();
    }
}

macro_rules! recv {
    ($p:pat => $e:expr) => {
        recv(move |request| {
            if let $p = request {
                $e
            } else {
                send(&Log(format_args!("unexpected reply: {:?}\n", request)));
                loop {}
            }
        })
    }
}

#[no_mangle] // https://github.com/rust-lang/rust/issues/{38281,51647}
#[panic_handler]
pub fn panic_fmt(info: &core::panic::PanicInfo) -> ! {
    if let Some(location) = info.location() {
        send(&Log(format_args!("panic at {}:{}:{}",
                               location.file(), location.line(), location.column())));
    } else {
        send(&Log(format_args!("panic at unknown location")));
    }
    if let Some(message) = info.message() {
        send(&Log(format_args!(": {}\n", message)));
    } else {
        send(&Log(format_args!("\n")));
    }
    send(&RunAborted);
    loop {}
}

macro_rules! print {
    ($($arg:tt)*) => ($crate::send(&$crate::kernel_proto::Log(format_args!($($arg)*))));
}

macro_rules! println {
    ($fmt:expr) => (print!(concat!($fmt, "\n")));
    ($fmt:expr, $($arg:tt)*) => (print!(concat!($fmt, "\n"), $($arg)*));
}

macro_rules! raise {
    ($name:expr, $message:expr, $param0:expr, $param1:expr, $param2:expr) => ({
        use cslice::AsCSlice;
        let name_id = $crate::eh_artiq::get_exception_id($name);
        let exn = $crate::eh_artiq::Exception {
            id:       name_id,
            file:     file!().as_c_slice(),
            line:     line!(),
            column:   column!(),
            // https://github.com/rust-lang/rfcs/pull/1719
            function: "(Rust function)".as_c_slice(),
            message:  $message.as_c_slice(),
            param:    [$param0, $param1, $param2]
        };
        #[allow(unused_unsafe)]
        unsafe { $crate::eh_artiq::raise(&exn) }
    });
    ($name:expr, $message:expr) => ({
        raise!($name, $message, 0, 0, 0)
    });
}

mod eh_artiq;
mod api;
mod rtio;
mod nrt_bus;
mod cxp;
mod mem;

static mut LIBRARY: Option<Library<'static>> = None;

#[no_mangle]
pub extern fn send_to_core_log(text: CSlice<u8>) {
    match str::from_utf8(text.as_ref()) {
        Ok(s) => send(&LogSlice(s)),
        Err(e) => {
            send(&LogSlice(str::from_utf8(&text.as_ref()[..e.valid_up_to()]).unwrap()));
            send(&LogSlice("(invalid utf-8)\n"));
        }
    }
}

#[no_mangle]
pub extern fn send_to_rtio_log(text: CSlice<u8>) {
    rtio::log(text.as_ref())
}

extern fn rpc_send(service: u32, tag: &CSlice<u8>, data: *const *const ()) {
    while !rpc_queue::empty() {}
    send(&RpcSend {
        async:   false,
        service: service,
        tag:     tag.as_ref(),
        data:    data
    })
}

extern fn rpc_send_async(service: u32, tag: &CSlice<u8>, data: *const *const ()) {
    while rpc_queue::full() {}
    rpc_queue::enqueue(|mut slice| {
        let length = {
            let mut writer = Cursor::new(&mut slice[4..]);
            rpc_proto::send_args(&mut writer, service, tag.as_ref(), data, true)?;
            writer.position()
        };
        io::ProtoWrite::write_u32(&mut slice, length as u32)
    }).unwrap_or_else(|err| {
        assert!(err == io::Error::UnexpectedEnd);

        while !rpc_queue::empty() {}
        send(&RpcSend {
            async:   true,
            service: service,
            tag:     tag.as_ref(),
            data:    data
        })
    })
}


/// Receives the result from an RPC call into the given memory buffer.
///
/// To handle aggregate objects with an a priori unknown size and number of
/// sub-allocations (e.g. a list of list of lists, where, at each level, the number of
/// elements is not statically known), this function needs to be called in a loop:
///
/// On the first call, `slot` should be a buffer of suitable size and alignment for
/// the top-level return value (e.g. in the case of a list, the pointer/length pair).
/// A return value of zero indicates that the value has been completely received.
/// As long as the return value is positive, another allocation with the given number of
/// bytes is needed, so the function should be called again with such a buffer (aligned
/// to the maximum required for any of the possible types according to the target ABI).
///
/// If the RPC call resulted in an exception, it is reconstructed and raised.
extern "C-unwind" fn rpc_recv(slot: *mut ()) -> usize {
    send(&RpcRecvRequest(slot));
    recv!(&RpcRecvReply(ref result) => {
        match result {
            &Ok(alloc_size) => alloc_size,
            &Err(ref exception) =>
            unsafe {
                eh_artiq::raise(&eh_artiq::Exception {
                    id:       exception.id,
                    file:     exception.file,
                    line:     exception.line,
                    column:   exception.column,
                    function: exception.function,
                    message:  exception.message,
                    param:    exception.param
                })
            }
        }
    })
}

fn terminate(exceptions: &'static [Option<eh_artiq::Exception<'static>>],
             stack_pointers: &'static [eh_artiq::StackPointerBacktrace],
             backtrace: &mut [(usize, usize)]) -> ! {
    send(&RunException {
        exceptions,
        stack_pointers,
        backtrace
    });
    loop {}
}

extern fn cache_get<'a>(key: CSlice<u8>) -> *const CSlice<'a, i32> {
    send(&CacheGetRequest {
        key:   str::from_utf8(key.as_ref()).unwrap()
    });
    recv!(&CacheGetReply { value } => {
        value
    })
}

extern "C-unwind" fn cache_put(key: CSlice<u8>, list: &CSlice<i32>) {
    send(&CachePutRequest {
        key:   str::from_utf8(key.as_ref()).unwrap(),
        value: list.as_ref()
    });
    recv!(&CachePutReply { succeeded } => {
        if !succeeded {
            raise!("CacheError", "cannot put into a busy cache row")
        }
    })
}

const DMA_BUFFER_SIZE: usize = 64 * 1024;

struct DmaRecorder {
    active:   bool,
    data_len: usize,
    buffer:   [u8; DMA_BUFFER_SIZE],
}

static mut DMA_RECORDER: DmaRecorder = DmaRecorder {
    active:   false,
    data_len: 0,
    buffer:   [0; DMA_BUFFER_SIZE],
};

fn dma_record_flush() {
    unsafe {
        send(&DmaRecordAppend(&DMA_RECORDER.buffer[..DMA_RECORDER.data_len]));
        DMA_RECORDER.data_len = 0;
    }
}

extern "C-unwind" fn dma_record_start(name: CSlice<u8>) {
    let name = str::from_utf8(name.as_ref()).unwrap();

    unsafe {
        if DMA_RECORDER.active {
            raise!("DMAError", "DMA is already recording")
        }

        let library = LIBRARY.as_ref().unwrap();
        library.rebind(b"rtio_output",
                       dma_record_output as *const () as u32).unwrap();
        library.rebind(b"rtio_output_wide",
                       dma_record_output_wide as *const () as u32).unwrap();
        board_misoc::cache::flush_cpu_icache();

        DMA_RECORDER.active = true;
        send(&DmaRecordStart(name));
    }
}

extern "C-unwind" fn dma_record_stop(duration: i64, enable_ddma: bool) {
    unsafe {
        dma_record_flush();

        if !DMA_RECORDER.active {
            raise!("DMAError", "DMA is not recording")
        }

        let library = LIBRARY.as_ref().unwrap();
        library.rebind(b"rtio_output",
                       rtio::output as *const () as u32).unwrap();
        library.rebind(b"rtio_output_wide",
                       rtio::output_wide as *const () as u32).unwrap();
        board_misoc::cache::flush_cpu_icache();

        DMA_RECORDER.active = false;
        send(&DmaRecordStop {
            duration: duration as u64,
            enable_ddma: enable_ddma
        });
    }
}

#[inline(always)]
unsafe fn dma_record_output_prepare(timestamp: i64, target: i32,
                                    words: usize) -> &'static mut [u8] {
    // See gateware/rtio/dma.py.
    const HEADER_LENGTH: usize = /*length*/1 + /*channel*/3 + /*timestamp*/8 + /*address*/1;
    let length = HEADER_LENGTH + /*data*/words * 4;

    if DMA_RECORDER.buffer.len() - DMA_RECORDER.data_len < length {
        dma_record_flush()
    }

    let record = &mut DMA_RECORDER.buffer[DMA_RECORDER.data_len..
                                          DMA_RECORDER.data_len + length];
    DMA_RECORDER.data_len += length;

    let (header, data) = record.split_at_mut(HEADER_LENGTH);

    header.copy_from_slice(&[
        (length    >>  0) as u8,
        (target    >>  8) as u8,
        (target    >>  16) as u8,
        (target    >>  24) as u8,
        (timestamp >>  0) as u8,
        (timestamp >>  8) as u8,
        (timestamp >> 16) as u8,
        (timestamp >> 24) as u8,
        (timestamp >> 32) as u8,
        (timestamp >> 40) as u8,
        (timestamp >> 48) as u8,
        (timestamp >> 56) as u8,
        (target    >>  0) as u8,
    ]);

    data
}

extern fn dma_record_output(target: i32, word: i32) {
    unsafe {
        let timestamp = ((csr::rtio::now_hi_read() as i64) << 32) | (csr::rtio::now_lo_read() as i64);
        let data = dma_record_output_prepare(timestamp, target, 1);
        data.copy_from_slice(&[
            (word >>  0) as u8,
            (word >>  8) as u8,
            (word >> 16) as u8,
            (word >> 24) as u8,
        ]);
    }
}

extern fn dma_record_output_wide(target: i32, words: &CSlice<i32>) {
    assert!(words.len() <= 16); // enforce the hardware limit

    unsafe {
        let timestamp = ((csr::rtio::now_hi_read() as i64) << 32) | (csr::rtio::now_lo_read() as i64);
        let mut data = dma_record_output_prepare(timestamp, target, words.len());
        for word in words.as_ref().iter() {
            data[..4].copy_from_slice(&[
                (word >>  0) as u8,
                (word >>  8) as u8,
                (word >> 16) as u8,
                (word >> 24) as u8,
            ]);
            data = &mut data[4..];
        }
    }
}

extern fn dma_erase(name: CSlice<u8>) {
    let name = str::from_utf8(name.as_ref()).unwrap();

    send(&DmaEraseRequest { name: name });
}

#[repr(C)]
struct DmaTrace {
    duration: i64,
    address:  i32,
    uses_ddma: bool,
}

extern "C-unwind" fn dma_retrieve(name: CSlice<u8>) -> DmaTrace {
    let name = str::from_utf8(name.as_ref()).unwrap();

    send(&DmaRetrieveRequest { name: name });
    recv!(&DmaRetrieveReply { trace, duration, uses_ddma } => {
        match trace {
            Some(bytes) => Ok(DmaTrace {
                address:  bytes.as_ptr() as i32,
                duration: duration as i64,
                uses_ddma: uses_ddma,
            }),
            None => Err(())
        }
    }).unwrap_or_else(|()| {
        println!("DMA trace called {:?} not found", name);
        raise!("DMAError",
            "DMA trace not found");
    })
}

#[cfg(kernel_has_rtio_dma)]
extern "C-unwind" fn dma_playback(timestamp: i64, ptr: i32, _uses_ddma: bool) {
    assert!(ptr % 64 == 0);

    unsafe {
        csr::rtio_dma::base_address_write(ptr as u64);
        csr::rtio_dma::time_offset_write(timestamp as u64);

        csr::cri_con::selected_write(1);
        csr::rtio_dma::enable_write(1);
        #[cfg(has_drtio)]
        if _uses_ddma {
            send(&DmaStartRemoteRequest { id: ptr as i32, timestamp: timestamp });
        }
        while csr::rtio_dma::enable_read() != 0 {}
        csr::cri_con::selected_write(0);

        let error = csr::rtio_dma::error_read();
        if error != 0 {
            let timestamp = csr::rtio_dma::error_timestamp_read();
            let channel = csr::rtio_dma::error_channel_read();
            csr::rtio_dma::error_write(1);
            if error & 1 != 0 {
                raise!("RTIOUnderflow",
                    "RTIO underflow at channel {rtio_channel_info:0}, {1} mu",
                    channel as i64, timestamp as i64, 0);
            }
            if error & 2 != 0 {
                raise!("RTIODestinationUnreachable",
                    "RTIO destination unreachable, output, at channel {rtio_channel_info:0}, {1} mu",
                    channel as i64, timestamp as i64, 0);
            }
        }
    }

    #[cfg(has_drtio)]
    if _uses_ddma {
        send(&DmaAwaitRemoteRequest { id: ptr as i32 });
        recv!(&DmaAwaitRemoteReply { timeout, error, channel, timestamp } => {
            if timeout {
                raise!("DMAError",
                    "Error running DMA on satellite device, timed out waiting for results");
            }
            if error & 1 != 0 {
                raise!("RTIOUnderflow",
                    "RTIO underflow at channel {rtio_channel_info:0}, {1} mu",
                    channel as i64, timestamp as i64, 0);
            }
            if error & 2 != 0 {
                raise!("RTIODestinationUnreachable",
                    "RTIO destination unreachable, output, at channel {rtio_channel_info:0}, {1} mu",
                    channel as i64, timestamp as i64, 0);
            }
        });
    }
}

#[cfg(all(not(kernel_has_rtio_dma), not(has_rtio_dma)))]
extern "C-unwind" fn dma_playback(_timestamp: i64, _ptr: i32, _uses_ddma: bool) {
    unimplemented!("not(kernel_has_rtio_dma)")
}

// for satellite (has_rtio_dma but not in kernel)
#[cfg(all(not(kernel_has_rtio_dma), has_rtio_dma))]
extern "C-unwind" fn dma_playback(timestamp: i64, ptr: i32, _uses_ddma: bool) {
    // DDMA is always used on satellites, so the `uses_ddma` setting is ignored
    // StartRemoteRequest reused as "normal" start request
    send(&DmaStartRemoteRequest { id: ptr as i32, timestamp: timestamp });
    // skip awaitremoterequest - it's a given
    recv!(&DmaAwaitRemoteReply { timeout, error, channel, timestamp } => {
        if timeout {
            raise!("DMAError",
                "Error running DMA on satellite device, timed out waiting for results");
        }
        if error & 1 != 0 {
            raise!("RTIOUnderflow",
                "RTIO underflow at channel {rtio_channel_info:0}, {1} mu",
                channel as i64, timestamp as i64, 0);
        }
        if error & 2 != 0 {
            raise!("RTIODestinationUnreachable",
                "RTIO destination unreachable, output, at channel {rtio_channel_info:0}, {1} mu",
                channel as i64, timestamp as i64, 0);
        }
    });
}


extern "C-unwind" fn subkernel_load_run(id: u32, destination: u8, run: bool) {
    let timestamp = unsafe {
        ((csr::rtio::now_hi_read() as u64) << 32) | (csr::rtio::now_lo_read() as u64)
    };
    send(&SubkernelLoadRunRequest { 
        id: id, 
        destination: destination, 
        run: run, 
        timestamp: timestamp,
    });
    recv!(&SubkernelLoadRunReply { succeeded } => {
        if !succeeded {
            raise!("SubkernelError",
                "Error loading or running the subkernel");
        }
    });
}

extern "C-unwind" fn subkernel_await_finish(id: u32, timeout: i64) {
    send(&SubkernelAwaitFinishRequest { id: id, timeout: timeout });
    recv(move |request| {
        if let SubkernelAwaitFinishReply = request { }
        else if let SubkernelError(status) = request {
            match status {
                SubkernelStatus::IncorrectState => raise!("SubkernelError",
                    "Subkernel not running"),
                SubkernelStatus::Timeout => raise!("SubkernelError",
                    "Subkernel timed out"),
                SubkernelStatus::CommLost => raise!("SubkernelError",
                    "Lost communication with satellite"),
                SubkernelStatus::OtherError => raise!("SubkernelError",
                    "An error occurred during subkernel operation"),
                SubkernelStatus::Exception(e) => unsafe { crate::eh_artiq::raise(e) },
            }
        } else {
            send(&Log(format_args!("unexpected reply: {:?}\n", request)));
            loop {}
        }
    })
}

extern fn subkernel_send_message(id: u32, is_return: bool, destination: u8, 
    count: u8, tag: &CSlice<u8>, data: *const *const ()) {
    send(&SubkernelMsgSend { 
        id: id,
        destination: if is_return { None } else { Some(destination) },
        count: count,
        tag: tag.as_ref(),
        data: data 
    });
}

extern "C-unwind" fn subkernel_await_message(id: i32, timeout: i64, tags: &CSlice<u8>, min: u8, max: u8) -> u8 {
    send(&SubkernelMsgRecvRequest { id: id, timeout: timeout, tags: tags.as_ref() });
    recv(move |request| {
        if let SubkernelMsgRecvReply { count } = request {
            if count < &min || count > &max {
                raise!("SubkernelError",
                    "Received less or more arguments than expected");
            }
            *count
        } else if let SubkernelError(status) = request {
            match status {
                SubkernelStatus::IncorrectState => raise!("SubkernelError",
                    "Subkernel not running"),
                SubkernelStatus::Timeout => raise!("SubkernelError",
                    "Subkernel timed out"),
                SubkernelStatus::CommLost => raise!("SubkernelError",
                    "Lost communication with satellite"),
                SubkernelStatus::OtherError => raise!("SubkernelError",
                    "An error occurred during subkernel operation"),
                SubkernelStatus::Exception(e) => unsafe { crate::eh_artiq::raise(e) },
            }
        } else {
            send(&Log(format_args!("unexpected reply: {:?}\n", request)));
            loop {}
        }
    })
    // RpcRecvRequest should be called `count` times after this to receive message data
}

unsafe fn attribute_writeback(typeinfo: *const ()) {
    #[repr(C)]
    struct Attr {
        offset: usize,
        tag:    CSlice<'static, u8>,
        name:   CSlice<'static, u8>
    }

    #[repr(C)]
    struct Type {
        attributes: *const *const Attr,
        objects:    *const *const ()
    }

    let mut tys = typeinfo as *const *const Type;
    while !(*tys).is_null() {
        let ty = *tys;
        tys = tys.offset(1);

        let mut objects = (*ty).objects;
        while !(*objects).is_null() {
            let object = *objects;
            objects = objects.offset(1);

            let mut attributes = (*ty).attributes;
            while !(*attributes).is_null() {
                let attribute = *attributes;
                attributes = attributes.offset(1);

                if (*attribute).tag.len() > 0 {
                    rpc_send_async(0, &(*attribute).tag, [
                        &object as *const _ as *const (),
                        &(*attribute).name as *const _ as *const (),
                        (object as usize + (*attribute).offset) as *const ()
                    ].as_ptr());
                }
            }
        }
    }
}

#[global_allocator]
static mut ALLOC: alloc_list::ListAlloc = alloc_list::EMPTY;
static mut STACK_GUARD_BASE: usize = 0x0;

extern {
    static mut _fheap_1: u8;
    static mut _eheap_1: u8;
}

#[no_mangle]
pub unsafe fn main() {
    ALLOC.add_range(&mut _fheap_1, &mut _eheap_1);

    eh_artiq::reset_exception_buffer(KERNELCPU_PAYLOAD_ADDRESS);
    let image = slice::from_raw_parts_mut(kernel_proto::KERNELCPU_PAYLOAD_ADDRESS as *mut u8,
                                          kernel_proto::KERNELCPU_LAST_ADDRESS -
                                          kernel_proto::KERNELCPU_PAYLOAD_ADDRESS);

    let library = recv!(&LoadRequest(library) => {
        match Library::load(library, image, &api::resolve) {
            Err(error) => {
                send(&LoadReply(Err(error)));
                loop {}
            },
            Ok(library) => {
                send(&LoadReply(Ok(())));
                // Master kernel would just acknowledge kernel load
                // Satellites may send UpdateNow
                try_recv(move |msg| match msg {
                    UpdateNow(timestamp) => unsafe {
                        csr::rtio::now_hi_write((*timestamp >> 32) as u32);
                        csr::rtio::now_lo_write(*timestamp as u32);
                    }
                    _ => unreachable!()
                });
                library
            }
        }
    });

    let __bss_start = library.lookup(b"__bss_start").unwrap();
    let _end = library.lookup(b"_end").unwrap();
    let __modinit__ = library.lookup(b"__modinit__").unwrap();
    let typeinfo = library.lookup(b"typeinfo");
    let _sstack_guard = library.lookup(b"_sstack_guard").unwrap();

    LIBRARY = Some(library);

    ptr::write_bytes(__bss_start as *mut u8, 0, (_end - __bss_start) as usize);

    board_misoc::pmp::init_stack_guard(_sstack_guard as usize);
    STACK_GUARD_BASE = _sstack_guard as usize;
    board_misoc::cache::flush_cpu_dcache();
    board_misoc::cache::flush_cpu_icache();

    (mem::transmute::<u32, fn()>(__modinit__))();

    if let Some(typeinfo) = typeinfo {
        attribute_writeback(typeinfo as *const ());
    }

    // Make sure all async RPCs are processed before exiting.
    // Otherwise, if the comms and kernel CPU run in the following sequence:
    //
    //    comms                     kernel
    //    -----------------------   -----------------------
    //    check for async RPC
    //                              post async RPC
    //                              post RunFinished
    //    check for mailbox
    //
    // the async RPC would be missed.
    send(&RpcFlush);

    send(&RunFinished);

    loop {}
}

#[no_mangle]
pub unsafe extern "C-unwind" fn exception(_regs: *const u32) {
    let pc = mepc::read();
    let cause = mcause::read().cause();
    let mtval = mtval::read();
    if let mcause::Trap::Exception(mcause::Exception::LoadFault)
    | mcause::Trap::Exception(mcause::Exception::StoreFault) = cause
    {
        if mtval >= STACK_GUARD_BASE
            && mtval < (STACK_GUARD_BASE + board_misoc::pmp::STACK_GUARD_SIZE)
        {
            panic!("{:?} at PC {:#08x} in stack guard page ({:#08x}); stack overflow in user kernel code?",
                   cause, u32::try_from(pc).unwrap(), mtval);
        }
    }
    panic!("{:?} at PC {:#08x}, trap value {:#08x}", cause, u32::try_from(pc).unwrap(), mtval);
}

#[no_mangle]
pub extern "C-unwind" fn abort() {
    panic!("aborted")
}
