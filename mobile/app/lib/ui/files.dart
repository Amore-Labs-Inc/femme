import 'package:flutter/cupertino.dart';

/// The session folder's files. Stub: the files screen replaces this file.
class FilesScreen extends StatelessWidget {
  const FilesScreen({super.key, required this.sid});

  final String sid;

  @override
  Widget build(BuildContext context) => const CupertinoPageScaffold(
    navigationBar: CupertinoNavigationBar(middle: Text('Files')),
    child: Center(child: Text('files: coming')),
  );
}
