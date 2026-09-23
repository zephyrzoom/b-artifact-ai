// Element Plus 组件的轻量替身。
//
// 组件测试只关心**我们自己的逻辑**（校验、禁用条件、请求体），不关心 Element Plus
// 自己的渲染。所以这里用最小替身：既保留真实 DOM 结构（按钮能点、能读 disabled），
// 又不把 Element Plus 的运行时拉进 happy-dom（teleport / 尺寸观察在无头环境里很脆）。

import type { Component } from 'vue'

/** 通用替身：保留标题 / 默认 / 页脚 / 描述插槽，便于断言文本。 */
export function slotStub(name: string): Component {
  return {
    name,
    template:
      '<div><slot name="title" /><slot name="description" /><slot /><slot name="footer" /></div>',
  }
}

export const InputStub: Component = {
  name: 'ElInput',
  props: {
    modelValue: { default: '' },
    type: { type: String, default: 'text' },
    rows: { type: Number, default: 2 },
    placeholder: { type: String, default: '' },
  },
  emits: ['update:modelValue'],
  template: `<input :value="modelValue" @input="$emit('update:modelValue', $event.target.value)" />`,
}

export const ButtonStub: Component = {
  name: 'ElButton',
  props: {
    disabled: { type: Boolean, default: false },
    loading: { type: Boolean, default: false },
    type: { type: String, default: '' },
    text: { type: Boolean, default: false },
    size: { type: String, default: '' },
    icon: { type: [Object, Function], default: null },
  },
  emits: ['click'],
  template: `<button :disabled="disabled" @click="$emit('click')"><slot /></button>`,
}

/** PurgeDialog 用到的全部替身。 */
export const purgeStubs = {
  'el-dialog': slotStub('ElDialog'),
  'el-alert': slotStub('ElAlert'),
  'el-form': slotStub('ElForm'),
  'el-form-item': slotStub('ElFormItem'),
  'el-collapse': slotStub('ElCollapse'),
  'el-collapse-item': slotStub('ElCollapseItem'),
  'el-descriptions': slotStub('ElDescriptions'),
  'el-descriptions-item': slotStub('ElDescriptionsItem'),
  'el-input': InputStub,
  'el-button': ButtonStub,
}

/** 什么都不渲染的替身：带作用域插槽的列组件必须用它，否则 `row` 是 undefined。 */
export const opaqueStub = (name: string): Component => ({ name, template: '<div />' })

/** el-result 的标题 / 副标题是 **prop** 而不是插槽，必须显式渲染出来。 */
export const ResultStub: Component = {
  name: 'ElResult',
  props: {
    icon: { type: String, default: '' },
    title: { type: String, default: '' },
    subTitle: { type: String, default: '' },
  },
  template: '<div class="result"><div class="result-title">{{ title }}</div><div class="result-sub">{{ subTitle }}</div><slot /></div>',
}

/** AclEditor 用到的全部替身（表格的 scoped slot 不渲染行，故只断言表格外的内容）。 */
export const aclStubs = {
  ...purgeStubs,
  'el-table': opaqueStub('ElTable'),
  'el-table-column': opaqueStub('ElTableColumn'),
  'el-tag': slotStub('ElTag'),
  'el-select': slotStub('ElSelect'),
  'el-option': slotStub('ElOption'),
  'el-radio-group': slotStub('ElRadioGroup'),
  'el-radio': slotStub('ElRadio'),
  'el-switch': slotStub('ElSwitch'),
  'el-drawer': slotStub('ElDrawer'),
  'el-steps': slotStub('ElSteps'),
  'el-step': slotStub('ElStep'),
  'el-result': ResultStub,
}
