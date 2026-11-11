import 'dart:async';

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:url_launcher/url_launcher.dart';

import '../../state/box_flows.dart';
import '../theme.dart';
import '../widgets.dart';

/// "+ add" in Accounts: a Claude sign-in on the box (desktop `accounts/flow.rs` `login_*`). Starts
/// one, or picks up [link] when one is already in progress. True once the account was added.
/// Swiping the sheet away leaves the sign-in on the box (the section offers to finish it);
/// Cancel gives it up.
Future<bool> showAddAccount(
  BuildContext context,
  BoardReq req, {
  String? link,
}) async {
  final added = await showCupertinoSheet<bool>(
    context: context,
    scrollableBuilder: (_, _) => AddAccountSheet(req: req, link: link),
  );
  return added ?? false;
}

class AddAccountSheet extends StatefulWidget {
  const AddAccountSheet({super.key, required this.req, this.link});
  final BoardReq req;
  final String? link;

  @override
  State<AddAccountSheet> createState() => _AddAccountSheetState();
}

class _AddAccountSheetState extends State<AddAccountSheet> {
  late final LoginFlow _flow = LoginFlow(widget.req, link: widget.link)
    ..addListener(_changed);
  final _code = TextEditingController();
  Timer? _wait;

  @override
  void initState() {
    super.initState();
    if (widget.link == null) _flow.start();
  }

  void _changed() {
    if (!mounted) return;
    final waiting = _flow.phase == LoginPhase.waitingLink;
    if (waiting && _wait == null) {
      _wait = Timer.periodic(
        const Duration(seconds: 2),
        (_) => _flow.refresh(),
      );
    } else if (!waiting) {
      _wait?.cancel();
      _wait = null;
    }
    if (_flow.phase == LoginPhase.done) {
      actionHaptic();
      Navigator.of(context).pop(true);
      return;
    }
    setState(() {});
  }

  @override
  void dispose() {
    _wait?.cancel();
    _flow.dispose();
    _code.dispose();
    super.dispose();
  }

  Future<void> _cancel() async {
    final nav = Navigator.of(context);
    await _flow.cancel();
    nav.pop(false);
  }

  Future<void> _open(String url) async {
    final u = Uri.tryParse(url);
    if (u != null) await launchUrl(u, mode: LaunchMode.externalApplication);
  }

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final f = _flow;
    final link = f.link;
    final starting =
        f.phase == LoginPhase.starting || f.phase == LoginPhase.waitingLink;
    return CupertinoPageScaffold(
      backgroundColor: CupertinoColors.systemGroupedBackground,
      navigationBar: CupertinoNavigationBar(
        automaticallyImplyLeading: false,
        middle: const Text('Add Claude account'),
        leading: CupertinoButton(
          padding: EdgeInsets.zero,
          onPressed: f.phase == LoginPhase.cancelled ? null : _cancel,
          child: const Text('Cancel'),
        ),
      ),
      child: SafeArea(
        child: ListView(
          children: [
            CupertinoListSection.insetGrouped(
              header: const Text('1. Sign in with the new account'),
              children: [
                if (starting)
                  CupertinoListTile(
                    leading: const CupertinoActivityIndicator(),
                    title: Text(
                      'getting the sign-in link',
                      style: TextStyle(color: muted),
                    ),
                  )
                else if (link != null && f.phase != LoginPhase.ended) ...[
                  Padding(
                    padding: const EdgeInsets.fromLTRB(16, 10, 16, 4),
                    child: Text(
                      link,
                      maxLines: 2,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                        fontFamily: monoFamily,
                        fontFamilyFallback: monoFallback,
                        fontSize: 12,
                        color: muted,
                      ),
                    ),
                  ),
                  Row(
                    children: [
                      Expanded(
                        child: CupertinoButton(
                          onPressed: () => _open(link),
                          child: const Text('Open link'),
                        ),
                      ),
                      Expanded(
                        child: CupertinoButton(
                          onPressed: () {
                            Clipboard.setData(ClipboardData(text: link));
                            tapHaptic();
                          },
                          child: const Text('Copy'),
                        ),
                      ),
                    ],
                  ),
                ] else
                  CupertinoListTile(
                    title: const Text('Start again'),
                    leading: const Icon(CupertinoIcons.refresh),
                    onTap: f.busy ? null : f.start,
                  ),
              ],
            ),
            CupertinoListSection.insetGrouped(
              header: const Text('2. Paste the code the sign-in page shows'),
              footer: f.error == null
                  ? null
                  : Text(
                      f.error!,
                      style: const TextStyle(
                        fontSize: 13,
                        color: FemmeColors.bad,
                      ),
                    ),
              children: [
                Padding(
                  padding: const EdgeInsets.all(10),
                  child: CupertinoTextField(
                    controller: _code,
                    enabled: f.phase == LoginPhase.link,
                    autocorrect: false,
                    enableSuggestions: false,
                    placeholder: 'paste the code',
                    onSubmitted: (_) => f.submit(_code.text),
                  ),
                ),
                Padding(
                  padding: const EdgeInsets.fromLTRB(10, 0, 10, 10),
                  child: CupertinoButton.filled(
                    onPressed: f.phase == LoginPhase.link
                        ? () => f.submit(_code.text)
                        : null,
                    child: f.phase == LoginPhase.sending
                        ? const CupertinoActivityIndicator(
                            color: CupertinoColors.white,
                          )
                        : const Text('Add account'),
                  ),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }
}
