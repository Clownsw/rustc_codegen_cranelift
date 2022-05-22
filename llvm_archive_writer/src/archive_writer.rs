use std::collections::HashMap;
use std::io::{self, Cursor, Seek, Write};

use object::{Object, ObjectSymbol};

use crate::alignment::*;
use crate::archive::*;

// Derived from:
// * https://github.com/llvm/llvm-project/blob/8ef3e895ad8ab1724e2b87cabad1dacdc7a397a3/llvm/include/llvm/Object/ArchiveWriter.h
// * https://github.com/llvm/llvm-project/blob/8ef3e895ad8ab1724e2b87cabad1dacdc7a397a3/llvm/lib/Object/ArchiveWriter.cpp

//===- ArchiveWriter.h - ar archive file format writer ----------*- C++ -*-===//
//
// Part of the LLVM Project, under the Apache License v2.0 with LLVM Exceptions.
// See https://llvm.org/LICENSE.txt for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
//
//===----------------------------------------------------------------------===//

pub struct NewArchiveMember {
    pub(crate) buf: Vec<u8>,
    pub(crate) member_name: String,
    pub(crate) mtime: u64,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) perms: u32,
}

//===- ArchiveWriter.cpp - ar File Format implementation --------*- C++ -*-===//
//
// Part of the LLVM Project, under the Apache License v2.0 with LLVM Exceptions.
// See https://llvm.org/LICENSE.txt for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
//
//===----------------------------------------------------------------------===//

/*
Expected<NewArchiveMember>
NewArchiveMember::getOldMember(const object::Archive::Child &OldMember,
                               bool Deterministic) {
  Expected<llvm::MemoryBufferRef> BufOrErr = OldMember.getMemoryBufferRef();
  if (!BufOrErr)
    return BufOrErr.takeError();

  NewArchiveMember M;
  M.Buf = MemoryBuffer::getMemBuffer(*BufOrErr, false);
  M.MemberName = M.Buf->getBufferIdentifier();
  if (!Deterministic) {
    auto ModTimeOrErr = OldMember.getLastModified();
    if (!ModTimeOrErr)
      return ModTimeOrErr.takeError();
    M.ModTime = ModTimeOrErr.get();
    Expected<unsigned> UIDOrErr = OldMember.getUID();
    if (!UIDOrErr)
      return UIDOrErr.takeError();
    M.UID = UIDOrErr.get();
    Expected<unsigned> GIDOrErr = OldMember.getGID();
    if (!GIDOrErr)
      return GIDOrErr.takeError();
    M.GID = GIDOrErr.get();
    Expected<sys::fs::perms> AccessModeOrErr = OldMember.getAccessMode();
    if (!AccessModeOrErr)
      return AccessModeOrErr.takeError();
    M.Perms = AccessModeOrErr.get();
  }
  return std::move(M);
}
*/

/*
Expected<NewArchiveMember> NewArchiveMember::getFile(StringRef FileName,
                                                     bool Deterministic) {
  sys::fs::file_status Status;
  auto FDOrErr = sys::fs::openNativeFileForRead(FileName);
  if (!FDOrErr)
    return FDOrErr.takeError();
  sys::fs::file_t FD = *FDOrErr;
  assert(FD != sys::fs::kInvalidFile);

  if (auto EC = sys::fs::status(FD, Status))
    return errorCodeToError(EC);

  // Opening a directory doesn't make sense. Let it fail.
  // Linux cannot open directories with open(2), although
  // cygwin and *bsd can.
  if (Status.type() == sys::fs::file_type::directory_file)
    return errorCodeToError(make_error_code(errc::is_a_directory));

  ErrorOr<std::unique_ptr<MemoryBuffer>> MemberBufferOrErr =
      MemoryBuffer::getOpenFile(FD, FileName, Status.getSize(), false);
  if (!MemberBufferOrErr)
    return errorCodeToError(MemberBufferOrErr.getError());

  if (auto EC = sys::fs::closeFile(FD))
    return errorCodeToError(EC);

  NewArchiveMember M;
  M.Buf = std::move(*MemberBufferOrErr);
  M.MemberName = M.Buf->getBufferIdentifier();
  if (!Deterministic) {
    M.ModTime = std::chrono::time_point_cast<std::chrono::seconds>(
        Status.getLastModificationTime());
    M.UID = Status.getUser();
    M.GID = Status.getGroup();
    M.Perms = Status.permissions();
  }
  return std::move(M);
}
*/

fn is_darwin(kind: ArchiveKind) -> bool {
    matches!(kind, ArchiveKind::Darwin | ArchiveKind::Darwin64)
}

fn is_bsd_like(kind: ArchiveKind) -> bool {
    match kind {
        ArchiveKind::Gnu | ArchiveKind::Gnu64 => false,
        ArchiveKind::Bsd | ArchiveKind::Darwin | ArchiveKind::Darwin64 => true,
        ArchiveKind::Coff | ArchiveKind::AixBig => panic!("not supported for writing"),
    }
}

fn print_rest_of_member_header<W: Write>(
    w: &mut W,
    mtime: u64,
    uid: u32,
    gid: u32,
    perms: u32,
    size: u64,
) -> io::Result<()> {
    // The format has only 6 chars for uid and gid. Truncate if the provided
    // values don't fit.
    write!(w, "{:<12}{:<6}{:<6}{:<8o}{:<10}`\n", mtime, uid % 1000000, gid % 1000000, perms, size)
}

fn print_gnu_small_member_header<W: Write>(
    w: &mut W,
    name: String,
    mtime: u64,
    uid: u32,
    gid: u32,
    perms: u32,
    size: u64,
) -> io::Result<()> {
    write!(w, "{:<16}", name + "/")?;
    print_rest_of_member_header(w, mtime, uid, gid, perms, size)
}

fn print_bsd_member_header<W: Write>(
    w: &mut W,
    pos: u64,
    name: &str,
    mtime: u64,
    uid: u32,
    gid: u32,
    perms: u32,
    size: u64,
) -> io::Result<()> {
    let pos_after_header = pos + 60 + u64::try_from(name.len()).unwrap();
    // Pad so that even 64 bit object files are aligned.
    let pad = offset_to_alignment(pos_after_header, 8);
    let name_with_padding = u64::try_from(name.len()).unwrap() + pad;
    write!(w, "#1/{:<13}", name_with_padding)?;
    print_rest_of_member_header(w, mtime, uid, gid, perms, name_with_padding + size)?;
    write!(w, "{}", name)?;
    write!(w, "{nil:\0<pad$}", nil = "", pad = usize::try_from(pad).unwrap())
}

fn use_string_table(thin: bool, name: &str) -> bool {
    thin || name.len() >= 16 || name.contains('/')
}

fn is_64bit_kind(kind: ArchiveKind) -> bool {
    match kind {
        ArchiveKind::Gnu
        | ArchiveKind::Bsd
        | ArchiveKind::Darwin
        | ArchiveKind::Coff
        | ArchiveKind::AixBig => false,
        ArchiveKind::Darwin64 | ArchiveKind::Gnu64 => true,
    }
}

fn print_member_header<'m, W: Write, T: Write + Seek>(
    w: &mut W,
    pos: u64,
    string_table: &mut T,
    member_names: &mut HashMap<&'m str, u64>,
    kind: ArchiveKind,
    thin: bool,
    m: &'m NewArchiveMember,
    mtime: u64,
    size: u64,
) -> io::Result<()> {
    if is_bsd_like(kind) {
        return print_bsd_member_header(w, pos, &m.member_name, mtime, m.uid, m.gid, m.perms, size);
    }

    if !use_string_table(thin, &m.member_name) {
        return print_gnu_small_member_header(
            w,
            m.member_name.clone(),
            mtime,
            m.uid,
            m.gid,
            m.perms,
            size,
        );
    }

    write!(w, "/")?;
    let name_pos;
    if thin {
        name_pos = string_table.stream_position()?;
        write!(string_table, "{}/\n", m.member_name)?;
    } else {
        if let Some(&pos) = member_names.get(&*m.member_name) {
            name_pos = pos;
        } else {
            name_pos = string_table.stream_position()?;
            member_names.insert(&m.member_name, name_pos);
            write!(string_table, "{}/\n", m.member_name)?;
        }
    }
    write!(w, "{:<15}", name_pos)?;
    print_rest_of_member_header(w, mtime, m.uid, m.gid, m.perms, size)
}

struct MemberData<'a> {
    symbols: Vec<u64>,
    header: Vec<u8>,
    data: &'a [u8],
    padding: &'static [u8],
}

fn compute_string_table(names: &[u8]) -> MemberData<'_> {
    let size = u64::try_from(names.len()).unwrap();
    let pad = offset_to_alignment(size, 2);
    let mut header = Vec::new();
    write!(header, "{:<48}", "//").unwrap();
    write!(header, "{:<10}", size + pad).unwrap();
    write!(header, "`\n").unwrap();
    MemberData { symbols: vec![], header, data: names, padding: if pad != 0 { b"\n" } else { b"" } }
}

fn now(deterministic: bool) -> u64 {
    if !deterministic {
        todo!(); // FIXME
    }
    0
}

fn is_archive_symbol(sym: &object::read::Symbol<'_, '_>) -> bool {
    // FIXME return false on the equivalent of LLVM's SymbolRef::SF_FormatSpecific
    if !sym.is_global() {
        return false;
    }
    if !sym.is_definition() {
        return false;
    }
    true
}

fn print_n_bits<W: Write>(w: &mut W, kind: ArchiveKind, val: u64) -> io::Result<()> {
    if is_64bit_kind(kind) {
        w.write_all(&if is_bsd_like(kind) { u64::to_le_bytes(val) } else { u64::to_be_bytes(val) })
    } else {
        w.write_all(&if is_bsd_like(kind) {
            u32::to_le_bytes(u32::try_from(val).unwrap())
        } else {
            u32::to_be_bytes(u32::try_from(val).unwrap())
        })
    }
}

fn compute_symbol_table_size_and_pad(
    kind: ArchiveKind,
    num_syms: u64,
    offset_size: u64,
    string_table: &[u8],
) -> (u64, u64) {
    assert!(offset_size == 4 || offset_size == 8, "Unsupported offset_size");
    let mut size = offset_size; // Number of entries
    if is_bsd_like(kind) {
        size += num_syms * offset_size * 2; // Table
    } else {
        size += num_syms * offset_size; // Table
    }
    if is_bsd_like(kind) {
        size += offset_size; // byte count;
    }
    size += u64::try_from(string_table.len()).unwrap();
    // ld64 expects the members to be 8-byte aligned for 64-bit content and at
    // least 4-byte aligned for 32-bit content.  Opt for the larger encoding
    // uniformly.
    // We do this for all bsd formats because it simplifies aligning members.
    let pad = offset_to_alignment(size, if is_bsd_like(kind) { 8 } else { 2 });
    size += pad;
    (size, pad)
}

fn write_symbol_table_header<W: Write + Seek>(
    w: &mut W,
    kind: ArchiveKind,
    deterministic: bool,
    size: u64,
) -> io::Result<()> {
    if is_bsd_like(kind) {
        let name = if is_64bit_kind(kind) { "__.SYMDEF_64" } else { "__.SYMDEF" };
        let pos = w.stream_position()?;
        print_bsd_member_header(w, pos, name, now(deterministic), 0, 0, 0, size)
    } else {
        let name = if is_64bit_kind(kind) { "/SYM64" } else { "" };
        print_gnu_small_member_header(w, name.to_string(), now(deterministic), 0, 0, 0, size)
    }
}

fn write_symbol_table<W: Write + Seek>(
    w: &mut W,
    kind: ArchiveKind,
    deterministic: bool,
    members: &[MemberData<'_>],
    string_table: &[u8],
) -> io::Result<()> {
    // We don't write a symbol table on an archive with no members -- except on
    // Darwin, where the linker will abort unless the archive has a symbol table.
    if string_table.is_empty() && !is_darwin(kind) {
        return Ok(());
    }

    let num_syms = u64::try_from(members.iter().map(|m| m.symbols.len()).sum::<usize>()).unwrap();

    let offset_size = if is_64bit_kind(kind) { 8 } else { 4 };
    let (size, pad) = compute_symbol_table_size_and_pad(kind, num_syms, offset_size, string_table);
    write_symbol_table_header(w, kind, deterministic, size)?;

    let mut pos = w.stream_position()? + size;

    if is_bsd_like(kind) {
        print_n_bits(w, kind, num_syms * 2 * offset_size)?;
    } else {
        print_n_bits(w, kind, num_syms)?;
    }

    for m in members {
        for &string_offset in &m.symbols {
            if is_bsd_like(kind) {
                print_n_bits(w, kind, string_offset)?;
            }
            print_n_bits(w, kind, pos)?;
        }
        pos += u64::try_from(m.header.len() + m.data.len() + m.padding.len()).unwrap();
    }

    if is_bsd_like(kind) {
        print_n_bits(w, kind, u64::try_from(string_table.len()).unwrap())?;
    }

    w.write_all(string_table)?;

    write!(w, "{nil:\0<pad$}", nil = "", pad = usize::try_from(pad).unwrap())
}

fn get_symbols(
    buf: &[u8],
    sym_names: &mut Cursor<Vec<u8>>,
    has_object: &mut bool,
) -> io::Result<Vec<u64>> {
    // FIXME match what LLVM does

    match object::File::parse(buf) {
        Ok(file) => {
            *has_object = true;
            let mut ret = vec![];
            for sym in file.symbols() {
                if !is_archive_symbol(&sym) {
                    continue;
                }
                ret.push(sym_names.stream_position()?);
                sym_names.write_all(sym.name_bytes().expect("FIXME"))?;
                sym_names.write_all(&[0])?;
            }
            Ok(ret)
        }
        Err(_) => Ok(vec![]),
    }
}

fn compute_member_data<'a, S: Write + Seek>(
    string_table: &mut S,
    sym_names: &mut Cursor<Vec<u8>>,
    kind: ArchiveKind,
    thin: bool,
    determinsitic: bool,
    need_symbols: bool,
    new_members: &'a [NewArchiveMember],
) -> io::Result<Vec<MemberData<'a>>> {
    const PADDING_DATA: &[u8; 8] = &[b'\n'; 8];

    // This ignores the symbol table, but we only need the value mod 8 and the
    // symbol table is aligned to be a multiple of 8 bytes
    let mut pos = 0;

    let mut ret = vec![];
    let mut has_object = false;

    // Deduplicate long member names in the string table and reuse earlier name
    // offsets. This especially saves space for COFF Import libraries where all
    // members have the same name.
    let mut member_names = HashMap::<&str, u64>::new();

    // UniqueTimestamps is a special case to improve debugging on Darwin:
    //
    // The Darwin linker does not link debug info into the final
    // binary. Instead, it emits entries of type N_OSO in in the output
    // binary's symbol table, containing references to the linked-in
    // object files. Using that reference, the debugger can read the
    // debug data directly from the object files. Alternatively, an
    // invocation of 'dsymutil' will link the debug data from the object
    // files into a dSYM bundle, which can be loaded by the debugger,
    // instead of the object files.
    //
    // For an object file, the N_OSO entries contain the absolute path
    // path to the file, and the file's timestamp. For an object
    // included in an archive, the path is formatted like
    // "/absolute/path/to/archive.a(member.o)", and the timestamp is the
    // archive member's timestamp, rather than the archive's timestamp.
    //
    // However, this doesn't always uniquely identify an object within
    // an archive -- an archive file can have multiple entries with the
    // same filename. (This will happen commonly if the original object
    // files started in different directories.) The only way they get
    // distinguished, then, is via the timestamp. But this process is
    // unable to find the correct object file in the archive when there
    // are two files of the same name and timestamp.
    //
    // Additionally, timestamp==0 is treated specially, and causes the
    // timestamp to be ignored as a match criteria.
    //
    // That will "usually" work out okay when creating an archive not in
    // deterministic timestamp mode, because the objects will probably
    // have been created at different timestamps.
    //
    // To ameliorate this problem, in deterministic archive mode (which
    // is the default), on Darwin we will emit a unique non-zero
    // timestamp for each entry with a duplicated name. This is still
    // deterministic: the only thing affecting that timestamp is the
    // order of the files in the resultant archive.
    //
    // See also the functions that handle the lookup:
    // in lldb: ObjectContainerBSDArchive::Archive::FindObject()
    // in llvm/tools/dsymutil: BinaryHolder::GetArchiveMemberBuffers().
    let unique_timestamps = determinsitic && is_darwin(kind);
    let mut filename_count = HashMap::new();
    if unique_timestamps {
        for m in new_members {
            *filename_count.entry(&*m.member_name).or_insert(0) += 1;
        }
        for (_name, count) in filename_count.iter_mut() {
            if *count > 1 {
                *count = 1;
            }
        }
    }

    for m in new_members {
        let mut header = Vec::new();

        let data = if thin { &[][..] } else { &m.buf };

        // ld64 expects the members to be 8-byte aligned for 64-bit content and at
        // least 4-byte aligned for 32-bit content.  Opt for the larger encoding
        // uniformly.  This matches the behaviour with cctools and ensures that ld64
        // is happy with archives that we generate.
        let member_padding = if is_darwin(kind) {
            offset_to_alignment(u64::try_from(data.len()).unwrap(), 8)
        } else {
            0
        };
        let tail_padding =
            offset_to_alignment(u64::try_from(data.len()).unwrap() + member_padding, 2);
        let padding = &PADDING_DATA[..usize::try_from(member_padding + tail_padding).unwrap()];

        let mtime = if unique_timestamps {
            // Increment timestamp for each file of a given name.
            *filename_count.get_mut(&*m.member_name).unwrap() += 1;
            filename_count[&*m.member_name] - 1
        } else {
            m.mtime
        };

        let size = u64::try_from(data.len()).unwrap() + member_padding;
        if size > MAX_MEMBER_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Archive member {} is too big", m.member_name),
            ));
        }

        print_member_header(
            &mut header,
            pos,
            string_table,
            &mut member_names,
            kind,
            thin,
            m,
            mtime,
            size,
        )?;

        let symbols =
            if need_symbols { get_symbols(data, sym_names, &mut has_object)? } else { vec![] };

        pos += u64::try_from(header.len() + data.len() + padding.len()).unwrap();
        ret.push(MemberData { symbols, header, data, padding })
    }

    // If there are no symbols, emit an empty symbol table, to satisfy Solaris
    // tools, older versions of which expect a symbol table in a non-empty
    // archive, regardless of whether there are any symbols in it.
    if has_object && sym_names.stream_position()? == 0 {
        write!(sym_names, "\0\0\0")?;
    }

    Ok(ret)
}

pub fn write_archive_to_stream<W: Write + Seek>(
    w: &mut W,
    new_members: &[NewArchiveMember],
    write_symtab: bool,
    mut kind: ArchiveKind,
    deterministic: bool,
    thin: bool,
) -> io::Result<()> {
    assert!(!thin || !is_bsd_like(kind), "Only the gnu format has a thin mode");

    let mut sym_names = Cursor::new(Vec::new());
    let mut string_table = Cursor::new(Vec::new());

    let mut data = compute_member_data(
        &mut string_table,
        &mut sym_names,
        kind,
        thin,
        deterministic,
        write_symtab,
        new_members,
    )?;

    let sym_names = sym_names.into_inner();

    let string_table = string_table.into_inner();
    if !string_table.is_empty() {
        data.insert(0, compute_string_table(&string_table));
    }

    // We would like to detect if we need to switch to a 64-bit symbol table.
    if write_symtab {
        let mut max_offset = 8; // For the file signature
        let mut last_offset = max_offset;
        let mut num_syms = 0;
        for m in &data {
            // Record the start of the member's offset
            last_offset = max_offset;
            // Account for the size of each part associated with the member.
            max_offset += u64::try_from(m.header.len() + m.data.len() + m.padding.len()).unwrap();
            num_syms += u64::try_from(m.symbols.len()).unwrap();
        }

        // We assume 32-bit offsets to see if 32-bit symbols are possible or not.
        let (symtab_size, _pad) = compute_symbol_table_size_and_pad(kind, num_syms, 4, &sym_names);
        last_offset += {
            // FIXME avoid allocating memory here
            let mut tmp = Cursor::new(vec![]);
            write_symbol_table_header(&mut tmp, kind, deterministic, symtab_size).unwrap();
            u64::try_from(tmp.into_inner().len()).unwrap()
        } + symtab_size;

        // The SYM64 format is used when an archive's member offsets are larger than
        // 32-bits can hold. The need for this shift in format is detected by
        // writeArchive. To test this we need to generate a file with a member that
        // has an offset larger than 32-bits but this demands a very slow test. To
        // speed the test up we use this environment variable to pretend like the
        // cutoff happens before 32-bits and instead happens at some much smaller
        // value.
        // FIXME allow lowering the threshold for tests
        const SYM64_THRESHOLD: u64 = 1 << 32;

        // If LastOffset isn't going to fit in a 32-bit varible we need to switch
        // to 64-bit. Note that the file can be larger than 4GB as long as the last
        // member starts before the 4GB offset.
        if last_offset >= SYM64_THRESHOLD {
            if kind == ArchiveKind::Darwin {
                kind = ArchiveKind::Darwin64;
            } else {
                kind = ArchiveKind::Gnu64;
            }
        }
    }

    if thin {
        write!(w, "!<thin>\n")?;
    } else {
        write!(w, "!<arch>\n")?;
    }

    if write_symtab {
        write_symbol_table(w, kind, deterministic, &data, &sym_names)?;
    }

    for m in data {
        w.write_all(&m.header)?;
        w.write_all(m.data)?;
        w.write_all(m.padding)?;
    }

    w.flush()
}

/*
Error writeArchive(StringRef ArcName, ArrayRef<NewArchiveMember> NewMembers,
                   bool WriteSymtab, object::Archive::Kind Kind,
                   bool Deterministic, bool Thin,
                   std::unique_ptr<MemoryBuffer> OldArchiveBuf) {
  Expected<sys::fs::TempFile> Temp =
      sys::fs::TempFile::create(ArcName + ".temp-archive-%%%%%%%.a");
  if (!Temp)
    return Temp.takeError();
  raw_fd_ostream Out(Temp->FD, false);

  if (Error E = writeArchiveToStream(Out, NewMembers, WriteSymtab, Kind,
                                     Deterministic, Thin)) {
    if (Error DiscardError = Temp->discard())
      return joinErrors(std::move(E), std::move(DiscardError));
    return E;
  }

  // At this point, we no longer need whatever backing memory
  // was used to generate the NewMembers. On Windows, this buffer
  // could be a mapped view of the file we want to replace (if
  // we're updating an existing archive, say). In that case, the
  // rename would still succeed, but it would leave behind a
  // temporary file (actually the original file renamed) because
  // a file cannot be deleted while there's a handle open on it,
  // only renamed. So by freeing this buffer, this ensures that
  // the last open handle on the destination file, if any, is
  // closed before we attempt to rename.
  OldArchiveBuf.reset();

  return Temp->keep(ArcName);
}
*/
