# 语义映射与区域排版

这两项操作复用原有 Rust/DeckCraft 流程。它们提供测量与候选，不决定审美，也不替代独立看图。

## 底图已有符号

先读原图和底图，判断箭头/运算符是否已经存在、是否只出现一次、方向及意思是否正确。混合句子先 `split_text`。再在第一次 `evidence_record` 冻结契约之前调用：

```json
{"action":"represent_symbol","spec":"source/split.json","output":"source/mapped.json",
 "page":"overview","id":"flow-arrow","box":[320,108,64,34],
 "basis":"原图与底图的两个标签之间都已有同方向的图形箭头"}
```

示例坐标仅说明 schema，不适用于实际稿件。工具将该原生符号的 `copy_id/copy_range` 转为页面 `artwork_symbols`，保留原文和精确字符覆盖、绑定底图哈希；不增加覆盖层，不修改图片。仅接受一个箭头或受支持运算符，拒绝文字、数字及多字符内容。该项不再是可编辑文字，交付时说明范围；用户要求所有符号可编辑时不得使用此路径。

`review_pack` 为它生成原图、底图、当前稿的对应裁图与三项判断：`representation`、`uniqueness`、`visibility`。映射说明和哈希不能证明语义等价；未审查的符号仍使内容检查保持 unknown。重复原生符号、缺失原文范围、过期底图都被工具拒绝。已冻结契约不能改映射绕过旧检查，禁止重置历史。

## 同组标签联合排版

先声明 `typography_groups`（含 `size_pt`）及每个标签自己的局部 `regions`；方向、四角和障碍从当前图像确定。整页或整块面板不是标签 region。未知空白区域先用已有 `layout_search` 辅助寻找，不能虚构 owner。

```json
{"action":"layout_region","spec":"source/deck.json","page":"overview",
 "group_id":"labels","sizes_pt":[18,16],"padding_px":4,
 "clearance_px":4,"max_lines":1,"output":"proposals/labels"}
```

工具从各 owner 推导刚性旋转和内边距，逐个共同字号调用现有字形测量，联合检查所有成员、其他原生文字、已映射符号与 owner 中的 `forbidden` 区域。任何成员放不下或发生冲突，该候选整组不输出 edits。工具不会擅自缩小某一个标签，不改未选文字；共享 owner 影响未选文字时要求先划分局部区域。

可行候选有完整源文件、实际 PPTX 和 PNG。阅读预览后选一个完整 source，保留未改页，再跑 `cycle`。不能把“不满足约束”当成设计已穷尽；可重新分析区域分配、换行或共同字号，但不能忽略障碍。候选是建议，不是通过验收。

限制：间距使用保守的轴对齐字形外包框；透视/剪切不由刚性旋转解决。未声明的位图装饰不会自动获得语义，仍需查看原图/底图/成品上下文。不要以这些限制为由解释真实遮挡是有意设计。
