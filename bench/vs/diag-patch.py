"""Applied to a server tree before a diagnostic build: stripe 0's bitstream and a per-frame index on the
server, per-frame receive and decode times in the worker, read back through voxlStats.diag."""
p = 'crates/starfold-server/src/video.rs'
s = open(p).read()
a = '        let nparts = parts.len() as u8;\n'
assert s.count(a) == 1
s = s.replace(a, '        diag_frame(frame_id, all_key, paintover, total, &parts);\n' + a)
s += '''
fn diag_frame(id: u32, key: bool, paintover: bool, total: usize, parts: &[(usize, bool, Vec<u8>)]) {
    use std::io::Write;
    static OUT: std::sync::OnceLock<std::sync::Mutex<(std::fs::File, std::fs::File)>> = std::sync::OnceLock::new();
    let out = OUT.get_or_init(|| {
        std::sync::Mutex::new((
            std::fs::File::create("/tmp/vs-s0.h264").unwrap(),
            std::fs::File::create("/tmp/vs-idx.log").unwrap(),
        ))
    });
    let mut f = out.lock().unwrap();
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    let s0 = parts.iter().find(|p| p.0 == 0);
    if let Some(p) = s0 {
        let _ = f.0.write_all(&p.2[16..]);
    }
    let _ = writeln!(f.1, "{ms} {id} {} {} {} {total} {}", key as u8, paintover as u8, parts.len(), s0.map_or(0, |p| p.2.len() - 16));
}
'''
open(p, 'w').write(s)
p = 'packages/web/src/worker.ts'
s = open(p).read()
a = '	const first = !frames.has(h.frame);\n'
assert s.count(a) == 1
s = s.replace(a, a + '	if (h.stripe === 0) diagRows.push([h.frame, Date.now(), 0]);\n')
a = '			sd.ids.delete(pic.timestamp);\n'
assert s.count(a) == 1
s = s.replace(a, a + '			if (i === 0) { const row = diagRows.findLast((x) => x[0] === id); if (row) row[2] = Date.now(); }\n')
a = '		dropped: st.dropped,\n'
assert s.count(a) == 1
s = s.replace(a, a + '		diag: diagRows,\n')
s = 'const diagRows: number[][] = [];\n' + s
open(p, 'w').write(s)
