/// Assistant markdown: body 16 px, code in monospace on the panel colour, links open in the
/// browser.
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
import 'package:url_launcher/url_launcher.dart';

import 'look.dart';

/// Opens [href] outside the app; bare paths are ignored.
Future<void> openLink(String? href) async {
  if (href == null) return;
  final u = Uri.tryParse(href);
  if (u == null || !u.hasScheme) return;
  await launchUrl(u, mode: LaunchMode.externalApplication);
}

class Md extends StatelessWidget {
  const Md(this.text, {super.key, this.size = 16});
  final String text;
  final double size;

  @override
  Widget build(BuildContext context) {
    final theme = CupertinoTheme.of(context);
    final base = MarkdownStyleSheet.fromCupertinoTheme(theme);
    final body = TextStyle(fontSize: size, height: 1.4, color: ink(context));
    final code = mono(context, size: 13);
    final sheet = base.copyWith(
      p: body,
      listBullet: body,
      a: body.copyWith(
        color: theme.primaryColor,
        decoration: TextDecoration.underline,
      ),
      code: code.copyWith(backgroundColor: panel(context)),
      codeblockDecoration: BoxDecoration(
        color: panel(context),
        border: Border.all(color: ink(context), width: 1.2),
      ),
      codeblockPadding: const EdgeInsets.all(10),
      blockquoteDecoration: BoxDecoration(
        border: Border(left: BorderSide(color: muted(context), width: 3)),
      ),
      blockquote: body.copyWith(color: muted(context)),
      h1: body.copyWith(fontSize: size + 6, fontWeight: FontWeight.w700),
      h2: body.copyWith(fontSize: size + 4, fontWeight: FontWeight.w700),
      h3: body.copyWith(fontSize: size + 2, fontWeight: FontWeight.w700),
      h4: body.copyWith(fontWeight: FontWeight.w700),
      h5: body.copyWith(fontWeight: FontWeight.w700),
      h6: body.copyWith(fontWeight: FontWeight.w700),
      tableBody: body.copyWith(fontSize: size - 2),
      tableHead: body.copyWith(fontSize: size - 2, fontWeight: FontWeight.w700),
      tableBorder: TableBorder.all(color: muted(context), width: 1),
      horizontalRuleDecoration: BoxDecoration(
        border: Border(top: BorderSide(color: muted(context), width: 1)),
      ),
    );
    return MarkdownBody(
      data: text,
      styleSheet: sheet,
      onTapLink: (_, href, _) => openLink(href),
      softLineBreak: true,
    );
  }
}
