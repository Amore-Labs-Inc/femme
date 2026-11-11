/// `femme://` links the phone opens: a push notification's tap (`femme://session/<sid>`) and the
/// code `femmed pair` prints (`femme://pair?host=…&token=…`).
library;

import 'dart:async';
import 'dart:io' show Platform;

import 'package:app_links/app_links.dart';
import 'package:flutter/foundation.dart';

import '../net/ids.dart';
import 'pairing.dart';

sealed class DeepLink {
  const DeepLink();
}

/// Open one session's chat.
final class OpenSession extends DeepLink {
  const OpenSession(this.sid);
  final String sid;

  @override
  bool operator ==(Object other) => other is OpenSession && other.sid == sid;
  @override
  int get hashCode => sid.hashCode;
  @override
  String toString() => 'OpenSession($sid)';
}

/// Pair with a box; carries the parsed [PairLink].
final class Pair extends DeepLink {
  const Pair(this.link);
  final PairLink link;

  String get host => link.host;
  String get token => link.token;
  bool get tls => link.tls;
  String get name => link.name;

  @override
  String toString() => 'Pair($name)';
}

/// What [uri] asks for; null when it is not a `femme://` link the app knows. The session id must
/// be a ULID as femmed writes it (26 upper-case Crockford base32 characters).
DeepLink? parseDeepLink(Uri uri) {
  if (uri.scheme.toLowerCase() != 'femme') return null;
  switch (uri.host.toLowerCase()) {
    case 'session':
      final segs = uri.pathSegments.where((s) => s.isNotEmpty).toList();
      if (segs.length != 1) return null;
      final sid = segs.single;
      if (!Ulid.isValid(sid) || sid != sid.toUpperCase()) return null;
      return OpenSession(sid);
    case 'pair':
      final l = PairLink.parse(uri.toString());
      return l == null ? null : Pair(l);
  }
  return null;
}

/// The links the OS hands the app, parsed. `AppLinks().uriLinkStream` carries the link that
/// launched the app as well as later ones, so no separate `getInitialLink` call (it would
/// deliver the cold-start link twice). Off Android and iOS (Linux desktop, `flutter test`) and
/// when the plugin is missing, the stream is empty and never throws.
abstract final class DeepLinks {
  static Stream<DeepLink> platform() {
    if (kIsWeb || !(Platform.isAndroid || Platform.isIOS)) {
      return const Stream.empty();
    }
    final Stream<Uri> uris;
    try {
      uris = AppLinks().uriLinkStream;
    } catch (_) {
      return const Stream.empty();
    }
    return uris
        .handleError((Object _) {})
        .map(parseDeepLink)
        .where((l) => l != null)
        .cast<DeepLink>();
  }
}
