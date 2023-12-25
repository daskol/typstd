#import "template.typ": *
// #import "@preview/tablex:0.7.0": *

#set text(font: "Times New Roman")

#let lsp = smallcaps[LSP]
#let typst = smallcaps[Typst]

= Overview

== Initialization

In fact #typst and #lsp share the same elements of architecture. The most
important one is an abstraction of document/source synchronization.
