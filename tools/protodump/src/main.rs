//! Recovers `.proto` files from the serialized FileDescriptorProtos embedded in a binary.
//!
//!   protodump <StarCitizen.exe> <output dir> [include dir for imports the binary lacks]
//!
//! protoc-generated code stores each file as a byte array that starts with field 1 (the path), so
//! the scan looks for `0x0a <varint> <path>.proto`, walks the top-level fields to find where the
//! message ends, and decodes it. Afterwards the emitted text is recompiled and compared with the
//! embedded descriptors, which proves the rendering lost nothing but options.

use std::{collections::BTreeMap, fmt::Write as _, fs, path::Path, process::ExitCode};

use prost::Message;
use prost_types::{
    field_descriptor_proto::{Label, Type},
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorProto,
    ServiceDescriptorProto,
};

/// Imports that only provide options. Options are not rendered, so these go too.
const OPTION_ONLY_IMPORTS: [&str; 2] =
    ["google/api/annotations.proto", "protoc-gen-openapiv2/options/annotations.proto"];
const MAX_PATH_LEN: usize = 200;

fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// End offset of the message at `start`: the walk stops at the first byte that is not a valid
/// FileDescriptorProto field (the array's terminator or padding).
fn message_end(data: &[u8], start: usize) -> usize {
    let mut pos = start;
    let mut end = start;
    while pos < data.len() {
        let Some(tag) = read_varint(data, &mut pos) else { break };
        let field = tag >> 3;
        if !(1..=14).contains(&field) {
            break;
        }
        match tag & 7 {
            0 => {
                if read_varint(data, &mut pos).is_none() {
                    break;
                }
            }
            2 => {
                let Some(length) = read_varint(data, &mut pos) else { break };
                let Some(next) = pos.checked_add(length as usize).filter(|n| *n <= data.len()) else { break };
                pos = next;
            }
            _ => break,
        }
        end = pos;
    }
    end
}

fn scan(data: &[u8]) -> BTreeMap<String, FileDescriptorProto> {
    let mut files = BTreeMap::new();
    let mut offset = 0;
    while let Some(found) = data[offset..].windows(6).position(|w| w == b".proto") {
        let suffix = offset + found;
        offset = suffix + 6;
        // Look back for the field-1 tag whose length ends exactly at this suffix. Matching on the
        // length rather than on path characters matters: a length byte can itself be printable.
        let path_end = suffix + 6;
        for start in (path_end.saturating_sub(MAX_PATH_LEN)..suffix).rev() {
            if data[start] != 0x0a {
                continue;
            }
            let mut pos = start + 1;
            let Some(length) = read_varint(data, &mut pos) else { continue };
            if pos.checked_add(length as usize) != Some(path_end) {
                continue;
            }
            let end = message_end(data, start);
            let Ok(file) = FileDescriptorProto::decode(&data[start..end]) else { continue };
            let useful = !file.message_type.is_empty() || !file.enum_type.is_empty() || !file.service.is_empty();
            if useful && !file.package().is_empty() {
                let keep = files.get(file.name()).is_none_or(|old: &FileDescriptorProto| old.encoded_len() < file.encoded_len());
                if keep {
                    files.insert(file.name().to_string(), file);
                }
            }
            break;
        }
    }
    files
}

fn scalar(kind: Type) -> Option<&'static str> {
    Some(match kind {
        Type::Double => "double",
        Type::Float => "float",
        Type::Int64 => "int64",
        Type::Uint64 => "uint64",
        Type::Int32 => "int32",
        Type::Fixed64 => "fixed64",
        Type::Fixed32 => "fixed32",
        Type::Bool => "bool",
        Type::String => "string",
        Type::Bytes => "bytes",
        Type::Uint32 => "uint32",
        Type::Sfixed32 => "sfixed32",
        Type::Sfixed64 => "sfixed64",
        Type::Sint32 => "sint32",
        Type::Sint64 => "sint64",
        Type::Group | Type::Message | Type::Enum => return None,
    })
}

fn type_name(field: &FieldDescriptorProto) -> String {
    scalar(field.r#type()).map_or_else(|| field.type_name().to_string(), str::to_string)
}

fn field_suffix(field: &FieldDescriptorProto) -> String {
    let deprecated = field.options.as_ref().is_some_and(|o| o.deprecated());
    format!("{} = {}{};", field.name(), field.number(), if deprecated { " [deprecated = true]" } else { "" })
}

fn render_enum(out: &mut String, item: &EnumDescriptorProto, indent: &str) {
    writeln!(out, "{indent}enum {} {{", item.name()).unwrap();
    if item.options.as_ref().is_some_and(|o| o.allow_alias()) {
        writeln!(out, "{indent}  option allow_alias = true;").unwrap();
    }
    for value in &item.value {
        writeln!(out, "{indent}  {} = {};", value.name(), value.number()).unwrap();
    }
    for range in &item.reserved_range {
        // Unlike message ranges, enum reserved ranges are stored with an inclusive end.
        writeln!(out, "{indent}  reserved {} to {};", range.start(), range.end()).unwrap();
    }
    for name in &item.reserved_name {
        writeln!(out, "{indent}  reserved \"{name}\";").unwrap();
    }
    writeln!(out, "{indent}}}").unwrap();
}

fn render_message(out: &mut String, message: &DescriptorProto, indent: &str, proto3: bool) {
    let inner = format!("{indent}  ");
    writeln!(out, "{indent}message {} {{", message.name()).unwrap();
    let maps: BTreeMap<&str, &DescriptorProto> = message
        .nested_type
        .iter()
        .filter(|n| n.options.as_ref().is_some_and(|o| o.map_entry()))
        .map(|n| (n.name(), n))
        .collect();

    let synthetic = |field: &FieldDescriptorProto| field.proto3_optional();
    for field in &message.field {
        if field.oneof_index.is_some() && !synthetic(field) {
            continue;
        }
        let entry = field.type_name().rsplit('.').next().and_then(|short| maps.get(short));
        if let (Some(entry), Label::Repeated) = (entry, field.label()) {
            let (key, value) = (&entry.field[0], &entry.field[1]);
            writeln!(out, "{inner}map<{}, {}> {}", type_name(key), type_name(value), field_suffix(field)).unwrap();
            continue;
        }
        let label = match field.label() {
            Label::Repeated => "repeated ",
            Label::Required => "required ",
            Label::Optional if synthetic(field) || !proto3 => "optional ",
            Label::Optional => "",
        };
        writeln!(out, "{inner}{label}{} {}", type_name(field), field_suffix(field)).unwrap();
    }
    for (index, oneof) in message.oneof_decl.iter().enumerate() {
        let members: Vec<_> = message
            .field
            .iter()
            .filter(|f| f.oneof_index == Some(index as i32) && !synthetic(f))
            .collect();
        if members.is_empty() {
            continue;
        }
        writeln!(out, "{inner}oneof {} {{", oneof.name()).unwrap();
        for field in members {
            writeln!(out, "{inner}  {} {}", type_name(field), field_suffix(field)).unwrap();
        }
        writeln!(out, "{inner}}}").unwrap();
    }
    for range in &message.reserved_range {
        writeln!(out, "{inner}reserved {} to {};", range.start(), range.end() - 1).unwrap();
    }
    for name in &message.reserved_name {
        writeln!(out, "{inner}reserved \"{name}\";").unwrap();
    }
    for nested in message.nested_type.iter().filter(|n| !maps.contains_key(n.name())) {
        render_message(out, nested, &inner, proto3);
    }
    for item in &message.enum_type {
        render_enum(out, item, &inner);
    }
    writeln!(out, "{indent}}}").unwrap();
}

fn render_service(out: &mut String, service: &ServiceDescriptorProto) {
    writeln!(out, "service {} {{", service.name()).unwrap();
    for method in &service.method {
        let stream = |on: bool| if on { "stream " } else { "" };
        writeln!(
            out,
            "  rpc {}({}{}) returns ({}{});",
            method.name(),
            stream(method.client_streaming()),
            method.input_type(),
            stream(method.server_streaming()),
            method.output_type()
        )
        .unwrap();
    }
    writeln!(out, "}}").unwrap();
}

fn render(file: &FileDescriptorProto, build: &str) -> String {
    let proto3 = file.syntax() == "proto3";
    let mut out = format!("// Generated by tools/protodump from {build}. Do not edit.\n");
    writeln!(out, "syntax = \"{}\";\npackage {};\n", if proto3 { "proto3" } else { "proto2" }, file.package()).unwrap();
    for import in file.dependency.iter().filter(|d| !OPTION_ONLY_IMPORTS.contains(&d.as_str())) {
        writeln!(out, "import \"{import}\";").unwrap();
    }
    out.push('\n');
    for message in &file.message_type {
        render_message(&mut out, message, "", proto3);
        out.push('\n');
    }
    for item in &file.enum_type {
        render_enum(&mut out, item, "");
        out.push('\n');
    }
    for service in &file.service {
        render_service(&mut out, service);
        out.push('\n');
    }
    for extension in &file.extension {
        let label = if extension.proto3_optional() { "optional " } else { "" };
        writeln!(out, "extend {} {{\n  {label}{} {}\n}}\n", extension.extendee(), type_name(extension), field_suffix(extension)).unwrap();
    }
    out
}

/// Drops what the text form intentionally omits so that the two sides compare equal.
fn normalize(file: &FileDescriptorProto) -> Vec<u8> {
    fn clean_message(m: &mut DescriptorProto) {
        for field in &mut m.field {
            field.json_name = None;
            let deprecated = field.options.as_ref().is_some_and(|o| o.deprecated());
            field.options = None;
            if deprecated {
                field.options = Some(prost_types::FieldOptions { deprecated: Some(true), ..Default::default() });
            }
        }
        m.field.sort_by_key(|f| f.number());
        m.nested_type.sort_by(|a, b| a.name().cmp(b.name()));
        m.enum_type.sort_by(|a, b| a.name().cmp(b.name()));
        m.nested_type.iter_mut().for_each(clean_message);
        m.enum_type.iter_mut().for_each(|e| {
            e.options = None;
            e.value.iter_mut().for_each(|v| v.options = None);
        });
        m.oneof_decl.iter_mut().for_each(|o| o.options = None);
        let map_entry = m.options.as_ref().is_some_and(|o| o.map_entry());
        m.options = map_entry.then(|| prost_types::MessageOptions { map_entry: Some(true), ..Default::default() });
    }
    let mut copy = file.clone();
    copy.options = None;
    copy.source_code_info = None;
    copy.dependency.retain(|d| !OPTION_ONLY_IMPORTS.contains(&d.as_str()));
    copy.public_dependency.clear();
    copy.weak_dependency.clear();
    copy.message_type.iter_mut().for_each(clean_message);
    copy.message_type.sort_by(|a, b| a.name().cmp(b.name()));
    copy.enum_type.iter_mut().for_each(|e| {
        e.options = None;
        e.value.iter_mut().for_each(|v| v.options = None);
    });
    copy.enum_type.sort_by(|a, b| a.name().cmp(b.name()));
    copy.extension.iter_mut().for_each(|e| {
        e.json_name = None;
        e.options = None;
    });
    copy.service.iter_mut().for_each(|s| {
        s.options = None;
        s.method.iter_mut().for_each(|m| m.options = None);
    });
    copy.encode_to_vec()
}

fn run(binary: &str, out_dir: &str, includes: &[String]) -> Result<(), String> {
    let data = fs::read(binary).map_err(|e| format!("read {binary}: {e}"))?;
    let build = Path::new(binary)
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("build_manifest.id"))
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| {
            text.lines()
                .find(|line| line.contains("RequestedP4ChangeNum"))
                .and_then(|line| line.split('"').nth(3))
                .map(|cl| format!("StarCitizen.exe CL {cl}"))
        })
        .unwrap_or_else(|| "StarCitizen.exe".to_string());

    let files = scan(&data);
    let ours: Vec<_> = files.values().filter(|f| f.name().starts_with("sc/")).collect();
    let services: usize = ours.iter().map(|f| f.service.len()).sum();
    let methods: usize = ours.iter().flat_map(|f| &f.service).map(|s| s.method.len()).sum();
    println!("found {} sc/ files, {services} services, {methods} methods", ours.len());

    for file in &ours {
        let path = Path::new(out_dir).join(file.name());
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&path, render(file, &build)).map_err(|e| format!("write {}: {e}", path.display()))?;
    }

    let mut roots = vec![out_dir.to_string()];
    roots.extend_from_slice(includes);
    let names: Vec<&str> = ours.iter().map(|f| f.name()).collect();
    let compiled = protox::compile(&names, &roots).map_err(|e| format!("recompile: {e:?}"))?;
    let mut differing = Vec::new();
    for file in &compiled.file {
        if let Some(original) = files.get(file.name()).filter(|f| f.name().starts_with("sc/")) {
            if normalize(file) != normalize(original) {
                differing.push(file.name().to_string());
            }
        }
    }
    if differing.is_empty() {
        println!("verified: all {} files recompile to the embedded descriptors", ours.len());
        Ok(())
    } else {
        Err(format!("{} files differ after recompiling: {differing:?}", differing.len()))
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: protodump <binary> <output dir> [include dir...]");
        return ExitCode::from(2);
    }
    match run(&args[0], &args[1], &args[2..]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}
