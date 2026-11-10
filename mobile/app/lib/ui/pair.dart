import 'package:flutter/cupertino.dart';

import '../state/prefs.dart';

/// First run: where the box is and the device token. (The QR pairing flow replaces the token
/// field once `PairStart`/`PairFinish` exist.)
class PairScreen extends StatefulWidget {
  const PairScreen({super.key, required this.prefs});
  final Prefs prefs;

  @override
  State<PairScreen> createState() => _PairScreenState();
}

class _PairScreenState extends State<PairScreen> {
  late final _host = TextEditingController(text: widget.prefs.host ?? '');
  final _token = TextEditingController();

  @override
  void dispose() {
    _host.dispose();
    _token.dispose();
    super.dispose();
  }

  bool get _ready =>
      _host.text.trim().isNotEmpty && _token.text.trim().isNotEmpty;

  @override
  Widget build(BuildContext context) => CupertinoPageScaffold(
    navigationBar: const CupertinoNavigationBar(
      middle: Text('Pair with your box'),
    ),
    child: SafeArea(
      child: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          const Text(
            'The box answers on wss://<host>/femme/ws. For a dev box, type ws://127.0.0.1:8787.',
          ),
          const SizedBox(height: 16),
          CupertinoTextField(
            controller: _host,
            placeholder: 'host',
            autocorrect: false,
            keyboardType: TextInputType.url,
            onChanged: (_) => setState(() {}),
          ),
          const SizedBox(height: 12),
          CupertinoTextField(
            controller: _token,
            placeholder: 'token',
            autocorrect: false,
            obscureText: true,
            onChanged: (_) => setState(() {}),
          ),
          const SizedBox(height: 24),
          CupertinoButton.filled(
            onPressed: _ready
                ? () => widget.prefs.setPairing(
                    _host.text.trim(),
                    _token.text.trim(),
                  )
                : null,
            child: const Text('Connect'),
          ),
        ],
      ),
    ),
  );
}
