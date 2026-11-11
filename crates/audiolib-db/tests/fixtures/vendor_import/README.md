# Original vendor-import fixtures

These are synthetic tables authored for AudioLib Explorer; they contain no user library data.
`basic.xls` is an actual BIFF8/OLE workbook with two sheets, generated once using xlwt 1.3.0.
`basic.xlsx` is an actual two-sheet OOXML workbook using inline strings. Both contain a
publisher preamble, a later header, Chinese text and a quoted/newline description.
The CSV variants cover UTF-16 LE/BE BOMs and four-byte GB18030 text. Fixture generation
does not add any Python or Office runtime dependency to the application or tests.
