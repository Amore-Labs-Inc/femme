import 'package:flutter/cupertino.dart';

import '../net/link.dart';
import 'femme.dart';
import 'pair.dart';
import 'widgets.dart';

/// What tapping a tab's link line does: the pairing screen when the box refused this device,
/// else the link sheet (full status, the last failed request, Reconnect).
Future<void> openLinkStatus(BuildContext context) async {
  tapHaptic();
  final f = Femme.of(context);
  if (f.link.state.value.notEnrolled) return pushPairAgain(context);
  final pair = await showCupertinoModalPopup<bool>(
    context: context,
    builder: (_) => LinkStatusSheet(link: f.link, host: f.prefs.host),
  );
  if (pair == true && context.mounted) await pushPairAgain(context);
}

/// The link's whole status, one fact per line: the phase line, the box, the last failed request.
/// The desktop shows this as the top bar's tooltip (`desktop/src/app.rs:303-305`).
String linkDetail(LinkState s, {String? host, DateTime? now}) {
  final f = s.lastFailed;
  return [
    linkLine(s),
    if (s.notEnrolled) 'the box refused this phone\'s token',
    if (host != null && host.isNotEmpty) 'box: $host',
    if (f != null) 'last failed: ${f.what}: ${f.why} (${_when(f.at, now)})',
  ].join('\n');
}

String _when(DateTime at, DateTime? now) {
  final a = ago(at, now: now);
  return a == 'now' ? 'just now' : '$a ago';
}

/// The link sheet: [linkDetail] live, then Reconnect (and Pair again when not enrolled).
class LinkStatusSheet extends StatelessWidget {
  const LinkStatusSheet({super.key, required this.link, this.host});
  final Link link;
  final String? host;

  @override
  Widget build(BuildContext context) => ValueListenableBuilder<LinkState>(
    valueListenable: link.state,
    builder: (context, s, _) => CupertinoActionSheet(
      title: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          ExcludeSemantics(child: StatusDot(linkColor(s))),
          const SizedBox(width: 8),
          const Text('Link'),
        ],
      ),
      message: Text(linkDetail(s, host: host), textAlign: TextAlign.left),
      actions: [
        if (s.notEnrolled)
          CupertinoActionSheetAction(
            isDefaultAction: true,
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('Pair again'),
          ),
        CupertinoActionSheetAction(
          onPressed: () {
            actionHaptic();
            link.reconnect();
            Navigator.of(context).pop();
          },
          child: const Text('Reconnect'),
        ),
      ],
      cancelButton: CupertinoActionSheetAction(
        onPressed: () => Navigator.of(context).pop(),
        child: const Text('Close'),
      ),
    ),
  );
}
