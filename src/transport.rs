//! low level transport that deals with reading bytes from an underlying Io
//! handling data split accross packets, etc.
use std::collections::VecDeque;
use std::cmp;
use std::fmt;
use std::io::{self, Write};
use std::mem;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::str;
use byteorder::{LittleEndian, ReadBytesExt};
use futures::{Async, Sink, StartSend, Poll};
use tokio_core::io::Io;
use protocol::{self, PacketHeader, PacketStatus, PacketType};
use tokens::{TdsResponseToken, Tokens, TokenColMetaData};
use {FromUint, TdsError};


pub struct TdsTransport<I: Io> {
    io: I,
    header: Option<PacketHeader>,
    /// whether the current token stream was read completely (EndOfMessage)
    completed: bool,
    requires_more: bool,
    missing: usize,
    hrd: [u8; protocol::HEADER_BYTES],
    pub rd: TdsBuf,
    wr: VecDeque<(usize, Vec<u8>)>,
    next_packet_id: u8,
    pub packet_size: usize,
    pub last_meta: Option<Arc<TokenColMetaData>>,
}

impl<I: Io> Deref for TdsTransport<I> {
    type Target = TdsBuf;

    fn deref(&self) -> &Self::Target {
        &self.rd
    }
}

impl<I: Io> DerefMut for TdsTransport<I> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rd
    }
}

pub trait ReadSize<R: io::Read> {
    fn read_size(&mut R) -> io::Result<usize>;
}

/// B_VARCHAR
impl<R: io::Read> ReadSize<R> for u8 {
     fn read_size(reader: &mut R) -> io::Result<usize> {
         Ok(try!(reader.read_u8()) as usize)
     }
}

/// US_VARCHAR
impl<R: io::Read> ReadSize<R> for u16 {
    fn read_size(reader: &mut R) -> io::Result<usize> {
         Ok(try!(reader.read_u16::<LittleEndian>()) as usize)
     }
}

/// TdsBuf/TdsBufMut inspired by tokio's EasyBuf
pub struct TdsBuf {
    start: usize,
    end: usize,
    //TODO: Arc or Rc?
    buf: Arc<Vec<u8>>,
}

impl TdsBuf {
    pub fn with_capacity(cap: usize) -> TdsBuf {
        TdsBuf {
            start: 0,
            end: 0,
            buf: Arc::new(Vec::with_capacity(cap)),
        }
    }

    pub fn position(&self) -> usize {
        self.start
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn as_str(&self) -> &str {
        // validation should've already happened in `read_varchar`
        // maybe use `from_utf8_unchecked` ?
        str::from_utf8(self.as_ref()).unwrap()
    }

    /// attempts to read n bytes and returns them as a subslice-buffer
    pub fn read_bytes(&mut self, n: usize) -> Option<TdsBuf> {
        if self.len() >= n {
            // determine whether it's the better option to copy or zero-copy
            // based on the buffer memory overhead
            let buf = if self.len() * 3/4 < n {
                // zero-copy
                TdsBuf {
                    start: self.start,
                    end: self.start + n,
                    buf: self.buf.clone(),
                }
            } else {
                // copy
                self.as_ref()[..n].to_owned().into()
            };
            self.start += n;
            return Some(buf)
        }
        None
    }

    /// read bytes with length prefix
    pub fn read_varbyte<S: ReadSize<Self>>(&mut self) -> Poll<TdsBuf, io::Error> {
        let len = try!(S::read_size(self));
        let ret = match self.read_bytes(len) {
            Some(bytes) => Async::Ready(bytes),
            None => Async::NotReady,
        };
        Ok(ret)
    }

    /// read bytes with 1/2*length prefix and interpret them as UCS-2 encoded string
    pub fn read_varchar<S: ReadSize<Self>>(&mut self) -> Poll<TdsBuf, TdsError> {
        let len = try!(S::read_size(self));
        // this is suboptimal but we need to copy them to be able to interpret these strings properly
        let data: Vec<u16> = try!(vec![0u16; len].into_iter().map(|_| self.read_u16::<LittleEndian>()).collect());
        let bytes = try!(String::from_utf16(&data[..])).into_bytes();;
        Ok(Async::Ready(bytes.into()))
    }

    /// get a mutable reference and
    // optionally ensure the underlying buffer has atleast a given length
    pub fn get_mut(&mut self, required_length: Option<usize>) -> (&mut Vec<u8>, usize) {
        // the underlying buffer is only used by us, we can get exclusive access
        if Arc::get_mut(&mut self.buf).is_some() {
            let buf = Arc::get_mut(&mut self.buf).unwrap();
            buf.drain(..self.start);
            self.end -= self.start;
            self.start = 0;
            if let Some(min_len) = required_length {
                if buf.len() < min_len + self.end {
                    buf.resize(min_len + self.end, 0);
                }
            }
            return (buf, self.end)
        }

        // can't get access, need a new buffer
        let mut new_capacity = self.buf.capacity();
        let min_capacity = self.len() + required_length.unwrap_or(0);
        if min_capacity > new_capacity {
           new_capacity = min_capacity;
        }
        // allocate a new buffer with the required length
        let mut v = Vec::with_capacity(new_capacity);
        v.extend_from_slice(self.as_ref());
        self.end -= v.len();
        if let Some(min_len) = required_length {
            if v.len() < min_len + self.end {
                v.resize(min_len + self.end, 0);
            }
        }
        self.start = 0;
        self.buf = Arc::new(v);
        let new_buf = Arc::get_mut(&mut self.buf).unwrap();
        (new_buf, self.end)
    }
}

impl io::Read for TdsBuf {
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        let len = cmp::min(buf.len(), self.len());
        let written = try!(buf.write(&self.as_ref()[..len]));
        if written == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "not enough bytes"));
        }
        self.start += written;
        Ok(written)
    }
}

impl AsRef<[u8]> for TdsBuf {
    fn as_ref(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }
}

impl From<Vec<u8>> for TdsBuf {
    fn from(v: Vec<u8>) -> TdsBuf {
        TdsBuf {
            start: 0,
            end: v.len(),
            buf: Arc::new(v),
        }
    }
}

impl fmt::Debug for TdsBuf {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match str::from_utf8(self.as_ref()) {
            Ok(str_) => write!(f, "{:?}", str_),
            Err(_) => write!(f, "TdsBuf({:?})", self.as_ref()),
        }
    }
}

impl<I: Io> TdsTransport<I> {
    pub fn new(io: I) -> TdsTransport<I> {
        let packet_size = 8192;
        TdsTransport {
            io: io,
            header: None,
            completed: false,
            requires_more: false,
            missing: protocol::HEADER_BYTES,
            hrd: [0; protocol::HEADER_BYTES],
            rd: TdsBuf::with_capacity(packet_size),
            wr: VecDeque::new(),
            //
            next_packet_id: 0,
            packet_size: packet_size,
            last_meta: None,
        }
    }

    /// get the next unused packet id
    #[inline]
    pub fn next_id(&mut self) -> u8 {
        let id = self.next_packet_id;
        self.next_packet_id = (id + 1) % 0xff;
        id
    }

    pub fn queue_vec(&mut self, buf: Vec<u8>) -> io::Result<()> {
        self.wr.push_back((0, buf));
        Ok(())
    }

    pub fn read_token(&mut self) -> Poll<Option<TdsResponseToken>, TdsError> {
        let old_pos = self.position();

        loop {
            let ret = (|| {
                if self.requires_more {
                    return Ok(Async::NotReady);
                }

                let token = Tokens::from_u8(match self.read_u8() {
                    Err(ref e) if e.kind() == ::std::io::ErrorKind::UnexpectedEof && self.completed => {
                        return Ok(Async::Ready(None));
                    },
                    x => try!(x)
                });

                // read the associated length for a token, if available
                let min_len = if let Some(ref token) = token {
                    match *token {
                        Tokens::SSPI | Tokens::EnvChange | Tokens::Info | Tokens::LoginAck => try!(self.read_u16::<LittleEndian>()) as usize,
                        _ => 0,
                    }
                } else { 0 };

                // check if we have enough data buffered (fast path)
                if min_len > self.len() {
                    return Ok(Async::NotReady)
                }

                match token {
                    Some(token) => Ok(Async::Ready(Some(try_ready!(self.parse_token(token, min_len))))),
                    None => panic!("invalid token received"),
                }
            })();
            let ret = match ret {
                Err(TdsError::Io(ref e)) if e.kind() == ::std::io::ErrorKind::UnexpectedEof && !self.completed => Ok(Async::NotReady),
                ret => ret,
            };
            match ret {
                Ok(Async::NotReady) => {
                    self.rd.start = old_pos;
                    self.requires_more = true;
                    let header = try_ready!(self.next_packet());
                    assert_eq!(header.ty, PacketType::TabularResult);
                    self.requires_more = false;
                },
                // reset the read buffer position
                ret @ Err(_) => {
                    self.rd.start = old_pos;
                    return ret
                },
                x => {
                    if self.as_ref().is_empty() {
                        self.completed = false;
                    }
                    return x
                }
            }
        }
    }

    /// simply returns a chunk of data with the specified length, adjusting read and write positions
    pub fn get_packet(&mut self, full_length: usize) -> TdsBuf {
        let len = full_length-protocol::HEADER_BYTES;
        self.read_bytes(len).unwrap()
    }

    /// buffers another packet from the underlying IO (or continues the last I/O operation)
    pub fn next_packet(&mut self) -> Poll<PacketHeader, TdsError> {
        // read the header first
        if self.header.is_none() {
            let offset = self.missing - protocol::HEADER_BYTES;

            while self.missing > 0 {
                if self.io.poll_read().is_not_ready() {
                    return Ok(Async::NotReady)
                }

                self.missing -= try_nb!(self.io.read(&mut self.hrd[offset..]))
            }

            let header = try!(PacketHeader::unserialize(&self.hrd));
            self.completed = header.status == PacketStatus::EndOfMessage;
            self.missing = header.length as usize - protocol::HEADER_BYTES;
            self.header = Some(header);
        }

        // read the packet body
        if self.header.is_some() {
            // make sure the packet body fits into the buffer
            while self.missing > 0 {
                if self.io.poll_read().is_not_ready() {
                    return Ok(Async::NotReady)
                }

                let count = {
                    let (write_buf, offset) = self.rd.get_mut(Some(self.missing));
                    try_nb!(self.io.read(&mut write_buf[offset..]))
                };
                self.rd.end += count;
                self.missing -= count;
            }

            // if we're done get ready to read the next packet and restore state
            self.missing = protocol::HEADER_BYTES;
            return Ok(Async::Ready(mem::replace(&mut self.header, None).unwrap()));
        }

        Ok(Async::NotReady)
    }
}

impl<I: Io> Sink for TdsTransport<I> {
    type SinkItem = ();
    type SinkError = io::Error;

    /// this is never used
    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        unimplemented!()
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        while !self.wr.is_empty() {
            if self.io.poll_write().is_not_ready() {
                return Ok(Async::NotReady)
            }
            let mut front_consumed = false;
            if let Some(ref mut front) = self.wr.front_mut() {
                let bytes = try!(self.io.write(&front.1[front.0..]));
                front.0 += bytes;
                if front.0 >= front.1.len() {
                    front_consumed = true;
                }
            }
            if front_consumed {
                self.wr.pop_front();
            }
            try_nb!(self.io.flush());
        }
        Ok(Async::Ready(()))
    }
}
