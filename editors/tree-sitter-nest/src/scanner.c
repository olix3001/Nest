// The tokens a regular expression cannot lex: a statement-ending newline, and a
// nesting block comment.
//
// A newline ends a statement wherever the grammar could end one there, which is
// what `nestc` decides in `filter_newlines`: a line ending on an operator never
// reaches here, because no statement can end after one, and a line **starting**
// with `.`, `+` or `::` continues the one before.

#include "tree_sitter/parser.h"

#include <stdbool.h>

enum TokenType {
  TERMINATOR,
  BLOCK_COMMENT,
  ERROR_SENTINEL,
};

void *tree_sitter_nest_external_scanner_create(void) { return NULL; }
void tree_sitter_nest_external_scanner_destroy(void *payload) {}
unsigned tree_sitter_nest_external_scanner_serialize(void *payload, char *buffer) { return 0; }
void tree_sitter_nest_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {}

static void advance(TSLexer *lexer) { lexer->advance(lexer, false); }
static void skip(TSLexer *lexer) { lexer->advance(lexer, true); }

static bool is_blank(int32_t c) { return c == ' ' || c == '\t' || c == '\r' || c == '\f'; }

// `/*` already consumed: the rest, counting depth.
static bool block_comment_rest(TSLexer *lexer) {
  unsigned depth = 1;
  while (!lexer->eof(lexer)) {
    if (lexer->lookahead == '*') {
      advance(lexer);
      if (lexer->lookahead == '/') {
        advance(lexer);
        if (--depth == 0) {
          lexer->mark_end(lexer);
          lexer->result_symbol = BLOCK_COMMENT;
          return true;
        }
      }
    } else if (lexer->lookahead == '/') {
      advance(lexer);
      if (lexer->lookahead == '*') {
        advance(lexer);
        depth++;
      }
    } else {
      advance(lexer);
    }
  }
  return false;
}

// Whether the text at the lexer, the start of the next non-blank line, carries
// on the statement before it.
static bool continues(TSLexer *lexer) {
  switch (lexer->lookahead) {
  case '.':
  case '+':
    return true;
  case ':':
    advance(lexer);
    return lexer->lookahead == ':';
  case 'e':
    // `else`, which nestc does not accept on a line of its own either; it is
    // taken here so a half-typed `if` does not fall apart.
    advance(lexer);
    if (lexer->lookahead != 'l') return false;
    advance(lexer);
    if (lexer->lookahead != 's') return false;
    advance(lexer);
    if (lexer->lookahead != 'e') return false;
    advance(lexer);
    return !(lexer->lookahead == '_' || (lexer->lookahead >= 'a' && lexer->lookahead <= 'z') ||
             (lexer->lookahead >= 'A' && lexer->lookahead <= 'Z') ||
             (lexer->lookahead >= '0' && lexer->lookahead <= '9'));
  default:
    return false;
  }
}

bool tree_sitter_nest_external_scanner_scan(void *payload, TSLexer *lexer, const bool *valid_symbols) {
  // Error recovery marks every symbol valid; a terminator guessed there only
  // makes the recovery worse.
  if (valid_symbols[ERROR_SENTINEL]) {
    return false;
  }

  if (valid_symbols[TERMINATOR]) {
    while (is_blank(lexer->lookahead)) skip(lexer);
    if (lexer->lookahead == '\n' || lexer->eof(lexer)) {
      if (lexer->eof(lexer)) {
        lexer->mark_end(lexer);
        lexer->result_symbol = TERMINATOR;
        return true;
      }
      while (lexer->lookahead == '\n' || is_blank(lexer->lookahead)) advance(lexer);
      lexer->mark_end(lexer);
      if (continues(lexer)) return false;
      lexer->result_symbol = TERMINATOR;
      return true;
    }
  } else {
    while (lexer->lookahead == '\n' || is_blank(lexer->lookahead)) skip(lexer);
  }

  if (valid_symbols[BLOCK_COMMENT] && lexer->lookahead == '/') {
    advance(lexer);
    if (lexer->lookahead != '*') return false;
    advance(lexer);
    return block_comment_rest(lexer);
  }
  return false;
}
