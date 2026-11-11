/// Assistant markdown: body 16 px, code in monospace in a rounded grey box with its language at
/// the top right, links open in the browser. Text still streaming is closed for drawing only:
/// an open fence is a code block to the end, a lone `**` is literal (desktop markdown.rs:1-8).
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

final _fenceOpen = RegExp(r'^ {0,3}(`{3,}|~{3,})(.*)$');

/// [text] as it should be drawn while it streams: an unclosed code fence is closed at the end
/// (a code block to the end, not a swallowed message), and a `**` left open in the last
/// paragraph is escaped so it shows as it is. Finished text comes back unchanged in effect.
String closeOpen(String text) {
  String? fence;
  var paraStart = 0;
  var offset = 0;
  final lines = text.split('\n');
  for (final line in lines) {
    if (fence == null) {
      final m = _fenceOpen.firstMatch(line);
      // a backtick fence's info string may not hold a backtick
      if (m != null && !(m[1]![0] == '`' && m[2]!.contains('`'))) {
        fence = m[1];
      } else if (line.trim().isEmpty) {
        paraStart = offset + line.length + 1;
      }
    } else {
      final t = line.trimLeft();
      if (line.length - t.length <= 3 &&
          t.startsWith(fence) &&
          t.replaceAll(fence[0], '').trim().isEmpty) {
        fence = null;
        paraStart = offset + line.length + 1;
      }
    }
    offset += line.length + 1;
  }
  if (fence != null) {
    return text.endsWith('\n') ? '$text$fence' : '$text\n$fence';
  }
  if (paraStart >= text.length) return text;
  final tail = text.substring(paraStart);
  // `**` outside inline code
  final marks = <int>[];
  var inCode = false;
  for (var i = 0; i < tail.length; i++) {
    final ch = tail[i];
    if (ch == '\\') {
      i++;
    } else if (ch == '`') {
      inCode = !inCode;
    } else if (!inCode && tail.startsWith('**', i)) {
      marks.add(i);
      i++;
    }
  }
  if (marks.length.isEven) return text;
  final at = paraStart + marks.last;
  return '${text.substring(0, at)}\\*\\*${text.substring(at + 2)}';
}

/// The language a fenced block names (`language-rust` on its `code` element), if any.
String? _langOf(Object? node) {
  try {
    final cls = (node as dynamic).attributes['class'];
    if (cls is String && cls.startsWith('language-')) {
      final l = cls.substring(9).trim();
      return l.isEmpty ? null : l;
    }
  } on NoSuchMethodError {
    // a text node: an indented block has no language
  }
  return null;
}

/// A code block's box: rounded 10 pt, the grouped secondary background, no border.
BoxDecoration codeBlockDecoration(BuildContext context) => BoxDecoration(
  color: CupertinoColors.secondarySystemBackground.resolveFrom(context),
  borderRadius: BorderRadius.circular(codeRadius),
);

/// A code block: its language small at the top right, the code scrolling sideways.
class _CodeBlock extends MarkdownElementBuilder {
  @override
  bool isBlockElement() => true;

  @override
  Widget? visitElementAfterWithContext(
    BuildContext context,
    element,
    TextStyle? preferredStyle,
    TextStyle? parentStyle,
  ) {
    final kids = element.children;
    final lang = kids == null || kids.isEmpty ? null : _langOf(kids.first);
    var code = element.textContent;
    if (code.endsWith('\n')) code = code.substring(0, code.length - 1);
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (lang != null)
          Container(
            padding: const EdgeInsets.fromLTRB(10, 4, 8, 4),
            decoration: BoxDecoration(
              border: Border(
                bottom: BorderSide(
                  color: separator(context),
                  width: hairline(context),
                ),
              ),
            ),
            child: Text(
              lang,
              textAlign: TextAlign.right,
              style: mono(context, size: 11, color: muted(context)),
            ),
          ),
        SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          padding: lang == null
              ? const EdgeInsets.all(10)
              : const EdgeInsets.fromLTRB(10, 8, 10, 10),
          child: Text(code, style: mono(context, size: 13)),
        ),
      ],
    );
  }
}

class Md extends StatelessWidget {
  const Md(this.text, {super.key, this.size = 16, this.streaming = false});
  final String text;
  final double size;

  /// The text is still being written: open fences and `**` are closed for drawing.
  final bool streaming;

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
      code: code.copyWith(backgroundColor: fill(context)),
      codeblockDecoration: codeBlockDecoration(context),
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
      tableBorder: TableBorder.all(
        color: separator(context),
        width: hairline(context),
      ),
      horizontalRuleDecoration: BoxDecoration(
        border: Border(
          top: BorderSide(color: separator(context), width: hairline(context)),
        ),
      ),
    );
    return MarkdownBody(
      data: streaming ? closeOpen(text) : text,
      styleSheet: sheet,
      builders: {'pre': _CodeBlock()},
      onTapLink: (_, href, _) => openLink(href),
      softLineBreak: true,
    );
  }
}
