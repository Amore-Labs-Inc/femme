/// Looks at bytes without decoding them: which picture format a blob is (magic bytes) and how big
/// the picture is (header fields). The desktop's `composer/sniff.rs`, ported.
library;

import 'dart:convert';
import 'dart:typed_data';

int _le16(Uint8List b, int at) => b[at] | b[at + 1] << 8;
int _le24(Uint8List b, int at) => b[at] | b[at + 1] << 8 | b[at + 2] << 16;
int _le32(Uint8List b, int at) => _le24(b, at) | b[at + 3] << 24;
int _be16(Uint8List b, int at) => b[at] << 8 | b[at + 1];
int _be32(Uint8List b, int at) =>
    b[at] << 24 | b[at + 1] << 16 | b[at + 2] << 8 | b[at + 3];

bool _starts(Uint8List b, List<int> magic) {
  if (b.length < magic.length) return false;
  for (var i = 0; i < magic.length; i++) {
    if (b[i] != magic[i]) return false;
  }
  return true;
}

bool _at(Uint8List b, int at, String ascii) {
  if (b.length < at + ascii.length) return false;
  for (var i = 0; i < ascii.length; i++) {
    if (b[at + i] != ascii.codeUnitAt(i)) return false;
  }
  return true;
}

/// The picture format of [b] by its magic bytes, as a file extension (`png`, `jpg`, `gif`,
/// `webp`, `bmp`, `svg`), or null for anything else.
String? pictureFormat(Uint8List b) {
  if (_starts(b, const [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])) {
    return 'png';
  }
  if (_starts(b, const [0xFF, 0xD8, 0xFF])) return 'jpg';
  if (_at(b, 0, 'GIF87a') || _at(b, 0, 'GIF89a')) return 'gif';
  if (_at(b, 0, 'RIFF') && _at(b, 8, 'WEBP')) return 'webp';
  if (_isBmp(b)) return 'bmp';
  if (_isSvg(b)) return 'svg';
  return null;
}

/// `BM`, four reserved zero bytes, then a DIB header of a size the format defines.
bool _isBmp(Uint8List b) =>
    b.length >= 26 &&
    _at(b, 0, 'BM') &&
    b[6] == 0 &&
    b[7] == 0 &&
    b[8] == 0 &&
    b[9] == 0 &&
    const {12, 40, 52, 56, 64, 108, 124}.contains(_le32(b, 14));

/// Text whose first element, after the XML declaration, doctype and comments, is `<svg`.
bool _isSvg(Uint8List b) {
  var s = utf8.decode(
    b.sublist(0, b.length < 4096 ? b.length : 4096),
    allowMalformed: true,
  );
  if (s.startsWith('\u{feff}')) s = s.substring(1);
  while (true) {
    s = s.trimLeft();
    String? end;
    if (s.startsWith('<?')) {
      end = '?>';
    } else if (s.startsWith('<!--')) {
      end = '-->';
    } else if (s.startsWith('<!')) {
      end = '>';
    } else {
      if (!s.startsWith('<svg')) return false;
      final r = s.substring(4);
      return r.isEmpty ||
          r.startsWith('>') ||
          r.startsWith('/') ||
          r[0].trim().isEmpty;
    }
    final i = s.indexOf(end, 2);
    if (i < 0) return false;
    s = s.substring(i + end.length);
  }
}

/// The pixel size of a picture read from its header, without decoding. Null for SVG and other
/// formats, cut-short bytes and a zero dimension.
({int w, int h})? pixelSize(Uint8List b) {
  final (int, int)? wh = switch (pictureFormat(b)) {
    'png' when b.length >= 24 && _at(b, 12, 'IHDR') => (
      _be32(b, 16),
      _be32(b, 20),
    ),
    'gif' when b.length >= 10 => (_le16(b, 6), _le16(b, 8)),
    'bmp' => _bmpSize(b),
    'webp' => _webpSize(b),
    'jpg' => _jpegSize(b),
    _ => null,
  };
  if (wh == null) return null;
  final (w, h) = wh;
  return w > 0 && h > 0 ? (w: w, h: h) : null;
}

(int, int) _bmpSize(Uint8List b) {
  if (_le32(b, 14) == 12) return (_le16(b, 18), _le16(b, 20));
  // The height is negative for a top-down bitmap.
  final d = ByteData.sublistView(b);
  return (
    d.getInt32(18, Endian.little).abs(),
    d.getInt32(22, Endian.little).abs(),
  );
}

(int, int)? _webpSize(Uint8List b) {
  if (b.length < 30) return null;
  // Lossy: frame tag, start code, then two 14-bit sizes.
  if (_at(b, 12, 'VP8 ') && b[23] == 0x9D && b[24] == 0x01 && b[25] == 0x2A) {
    return (_le16(b, 26) & 0x3FFF, _le16(b, 28) & 0x3FFF);
  }
  // Lossless: signature byte, then 14 bits each of width - 1 and height - 1.
  if (_at(b, 12, 'VP8L') && b[20] == 0x2F) {
    final bits = _le32(b, 21);
    return ((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1);
  }
  // Extended: canvas size minus one, 24 bits each.
  if (_at(b, 12, 'VP8X')) return (_le24(b, 24) + 1, _le24(b, 27) + 1);
  return null;
}

/// Walks the marker segments up to the first start-of-frame, which holds the size.
(int, int)? _jpegSize(Uint8List b) {
  var i = 2;
  while (i + 4 <= b.length) {
    if (b[i] != 0xFF) return null;
    final m = b[i + 1];
    if (m == 0xFF) {
      i += 1; // fill byte
    } else if (m == 0xD8 || m == 0x01 || (m >= 0xD0 && m <= 0xD7)) {
      i += 2;
    } else if (m == 0xD9 || m == 0xDA) {
      return null;
    } else if (m >= 0xC0 && m <= 0xCF && m != 0xC4 && m != 0xC8 && m != 0xCC) {
      return i + 9 <= b.length ? (_be16(b, i + 7), _be16(b, i + 5)) : null;
    } else {
      final len = _be16(b, i + 2);
      if (len < 2) return null;
      i += 2 + len;
    }
  }
  return null;
}
