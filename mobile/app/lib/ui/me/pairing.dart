import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:mobile_scanner/mobile_scanner.dart';

import '../../state/pairing.dart';
import '../femme.dart';
import '../theme.dart';
import '../widgets.dart';

/// Scan, paste, or type: the three ways to pair. Saving the pairing makes the app connect.
class PairForm extends StatefulWidget {
  const PairForm({super.key, this.onPaired, this.manual = false});

  /// Shows the host and token fields open (first run).
  final bool manual;

  /// Called after the pairing is saved (the Me tab pops its page).
  final VoidCallback? onPaired;

  @override
  State<PairForm> createState() => _PairFormState();
}

class _PairFormState extends State<PairForm> {
  final _link = TextEditingController();
  final _host = TextEditingController();
  final _token = TextEditingController();
  late bool _manual = widget.manual;
  String? _error;

  @override
  void dispose() {
    _link.dispose();
    _host.dispose();
    _token.dispose();
    super.dispose();
  }

  Future<void> _use(PairLink l) async {
    final prefs = Femme.of(context).prefs;
    actionHaptic();
    await PairInfo.save(prefs, l);
    widget.onPaired?.call();
  }

  Future<void> _scan() async {
    final l = await Navigator.of(context, rootNavigator: true).push<PairLink>(
      CupertinoPageRoute(
        fullscreenDialog: true,
        builder: (_) => const ScannerPage(),
      ),
    );
    if (l != null && mounted) await _use(l);
  }

  Future<void> _paste() async {
    final data = await Clipboard.getData(Clipboard.kTextPlain);
    final text = data?.text ?? '';
    _link.text = text;
    _tryLink(text);
  }

  void _tryLink(String text) {
    final l = PairLink.parse(text);
    if (l == null) {
      setState(
        () => _error = text.trim().isEmpty ? null : 'not a femme://pair link',
      );
      return;
    }
    setState(() => _error = null);
    _use(l);
  }

  Future<void> _connectManual() async {
    final prefs = Femme.of(context).prefs;
    actionHaptic();
    await PairInfo.saveManual(prefs, _host.text, _token.text);
    widget.onPaired?.call();
  }

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final manualReady =
        _host.text.trim().isNotEmpty && _token.text.trim().isNotEmpty;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        CupertinoListSection.insetGrouped(
          footer: sectionFooter(
            context,
            'Run `femmed pair --name <this phone>` on your box and scan the code it prints.',
          ),
          children: [
            CupertinoListTile(
              leading: const Icon(CupertinoIcons.qrcode_viewfinder),
              title: const Text('Scan QR code'),
              trailing: const CupertinoListTileChevron(),
              onTap: _scan,
            ),
          ],
        ),
        CupertinoListSection.insetGrouped(
          header: const Text('Or paste the link'),
          footer: _error == null
              ? null
              : Text(
                  _error!,
                  style: const TextStyle(fontSize: 13, color: FemmeColors.bad),
                ),
          children: [
            Padding(
              padding: const EdgeInsets.all(10),
              child: CupertinoTextField(
                controller: _link,
                placeholder: 'femme://pair?host=…&token=…',
                autocorrect: false,
                keyboardType: TextInputType.url,
                onSubmitted: _tryLink,
                suffix: CupertinoButton(
                  sizeStyle: CupertinoButtonSize.small,
                  onPressed: _paste,
                  child: const Text('Paste'),
                ),
              ),
            ),
          ],
        ),
        CupertinoListSection.insetGrouped(
          children: [
            CupertinoListTile(
              title: const Text('Enter host and token'),
              trailing: Icon(
                _manual
                    ? CupertinoIcons.chevron_up
                    : CupertinoIcons.chevron_down,
                size: 16,
                color: muted,
              ),
              onTap: () => setState(() => _manual = !_manual),
            ),
            if (_manual) ...[
              Padding(
                padding: const EdgeInsets.all(10),
                child: CupertinoTextField(
                  controller: _host,
                  placeholder:
                      'host (wss://box.example.com, ws://127.0.0.1:8787)',
                  autocorrect: false,
                  keyboardType: TextInputType.url,
                  onChanged: (_) => setState(() {}),
                ),
              ),
              Padding(
                padding: const EdgeInsets.all(10),
                child: CupertinoTextField(
                  controller: _token,
                  placeholder: 'token',
                  autocorrect: false,
                  obscureText: true,
                  onChanged: (_) => setState(() {}),
                ),
              ),
              Padding(
                padding: const EdgeInsets.all(10),
                child: CupertinoButton.filled(
                  onPressed: manualReady ? _connectManual : null,
                  child: const Text('Connect'),
                ),
              ),
            ],
          ],
        ),
      ],
    );
  }
}

/// The camera, until it sees a pairing link; pops with it.
class ScannerPage extends StatefulWidget {
  const ScannerPage({super.key});

  @override
  State<ScannerPage> createState() => _ScannerPageState();
}

class _ScannerPageState extends State<ScannerPage> {
  bool _done = false;
  String? _seen;

  void _detect(BarcodeCapture cap) {
    if (_done) return;
    for (final b in cap.barcodes) {
      final raw = b.rawValue;
      if (raw == null) continue;
      final l = PairLink.parse(raw);
      if (l != null) {
        _done = true;
        HapticFeedback.mediumImpact();
        Navigator.of(context).pop(l);
        return;
      }
      if (raw != _seen) setState(() => _seen = raw);
    }
  }

  @override
  Widget build(BuildContext context) => CupertinoPageScaffold(
    backgroundColor: CupertinoColors.black,
    navigationBar: CupertinoNavigationBar(
      automaticallyImplyLeading: false,
      middle: const Text('Scan pairing code'),
      leading: CupertinoButton(
        padding: EdgeInsets.zero,
        onPressed: () => Navigator.of(context).pop(),
        child: const Text('Cancel'),
      ),
    ),
    child: Stack(
      fit: StackFit.expand,
      children: [
        MobileScanner(
          onDetect: _detect,
          errorBuilder: (context, error) => Center(
            child: Padding(
              padding: const EdgeInsets.all(24),
              child: Text(
                'The camera is not available: ${error.errorCode.name}. '
                'Paste the link instead.',
                textAlign: TextAlign.center,
                style: const TextStyle(color: CupertinoColors.white),
              ),
            ),
          ),
        ),
        Center(
          child: Container(
            width: 240,
            height: 240,
            decoration: BoxDecoration(
              border: Border.all(color: FemmeColors.neon, width: 3),
            ),
          ),
        ),
        if (_seen != null)
          Positioned(
            left: 16,
            right: 16,
            bottom: 48,
            child: Text(
              'That code is not a FEMME pairing link.',
              textAlign: TextAlign.center,
              style: const TextStyle(color: CupertinoColors.white),
            ),
          ),
      ],
    ),
  );
}
