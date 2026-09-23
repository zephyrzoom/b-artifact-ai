// Element Plus 的轻量替身（同 admin/tests/helpers/stubs.ts 的思路）：
// 只保留真实 DOM 语义（按钮能点、能读 disabled、输入能改），
// 不把 Element Plus 运行时拉进 happy-dom。

import { h } from 'vue';
import type { Component } from 'vue';

export function slotStub(name: string): Component {
  return {
    name,
    template:
      '<div><slot name="title" /><slot name="description" /><slot name="header" /><slot /><slot name="footer" /></div>',
  };
}

export function opaqueStub(name: string): Component {
  return { name, template: '<div />' };
}

/** 数字输入：真实 Element Plus 的根元素就是 input，这里保持一致。 */
export const InputNumberStub: Component = {
  name: 'ElInputNumber',
  props: { modelValue: { type: Number, default: 0 }, min: Number, max: Number },
  emits: ['update:modelValue'],
  template: `<input type="number" :value="modelValue"
    @input="$emit('update:modelValue', Number($event.target.value))" />`,
};

export const InputStub: Component = {
  name: 'ElInput',
  props: { modelValue: { default: '' }, type: { type: String, default: 'text' } },
  // 真实 el-input 两个事件都发：`update:modelValue`（v-model）与 `input`（业务侧监听"用户改过"）
  emits: ['update:modelValue', 'input'],
  // 多行输入必须真的渲染 <textarea>：否则测试断言不到"这是一个多行框"，
  // 也与真实 Element Plus（type=textarea 时根元素就是 textarea）不一致。
  template: `
    <textarea v-if="type === 'textarea'" :value="modelValue"
      @input="$emit('update:modelValue', $event.target.value); $emit('input', $event.target.value)"></textarea>
    <input v-else :value="modelValue"
      @input="$emit('update:modelValue', $event.target.value); $emit('input', $event.target.value)" />
  `,
};

export const ButtonStub: Component = {
  name: 'ElButton',
  props: {
    disabled: { type: Boolean, default: false },
    loading: { type: Boolean, default: false },
    type: { type: String, default: '' },
    size: { type: String, default: '' },
    text: { type: Boolean, default: false },
    plain: { type: Boolean, default: false },
    icon: { type: [Object, Function], default: null },
  },
  emits: ['click'],
  // 透传原生事件：组件的 `@click.stop` 需要它（真实 el-button 也是把原生事件交出去）
  template: `<button :disabled="disabled" @click="$emit('click', $event)"><slot /></button>`,
};

/** el-alert 的标题是 **prop** 而不是插槽，替身必须显式渲染出来才有文本可断言。 */
export const AlertStub: Component = {
  name: 'ElAlert',
  props: {
    title: { type: String, default: '' },
    type: { type: String, default: '' },
    closable: { type: Boolean, default: true },
    showIcon: { type: Boolean, default: false },
  },
  emits: ['close'],
  template: '<div class="alert">{{ title }}<slot /></div>',
};

/** el-empty 的 description 同样是 prop。 */
/**
 * 表格替身：**真的按"行 × 列"渲染**。
 *
 * 只渲染 `<slot />` 是不够的：`el-table-column` 的默认插槽（`#default="{ row }"`）永远
 * 不会被执行，于是"列里长出来的按钮/文案"在单测里根本不存在，表格类视图几乎没法测。
 * 这里用 provide/inject 把当前行传给每个列，列再把它交给自己的插槽 —— 与真实 el-table
 * 的 `{ row }` 作用域一致。
 */
const ROW_KEY = 'stub-table-row';

const RowProvider: Component = {
  props: { row: { type: Object, default: () => ({}) } },
  emits: ['row-click'],
  provide() {
    return { [ROW_KEY]: (this as unknown as { row: unknown }).row };
  },
  // 点行要能冒到表格（真实 el-table 的 `@row-click`）：用来测"整行可点即打开"
  template: "<div class=\"table-row\" @click=\"$emit('row-click', row)\"><slot /></div>",
};

export const TableColumnStub: Component = {
  name: 'ElTableColumn',
  props: {
    label: { type: String, default: '' },
    prop: { type: String, default: '' },
    width: { type: [String, Number], default: '' },
    minWidth: { type: [String, Number], default: '' },
    // `fixed="right"` 是"操作列不被滚出视野"的手段，测试要能断言它
    fixed: { type: [String, Boolean], default: false },
    showOverflowTooltip: { type: Boolean, default: false },
  },
  inject: { row: { from: ROW_KEY, default: undefined } },
  computed: {
    /** 没有默认插槽时退化成"取 prop 字段"，与真实列一致。 */
    text(): string {
      const row = (this as unknown as { row: Record<string, unknown> | undefined }).row;
      const prop = (this as unknown as { prop: string }).prop;
      return row && prop ? String(row[prop] ?? '') : '';
    },
  },
  template:
    '<div class="table-cell" :data-label="label"><slot :row="row" :$index="0">{{ text }}</slot></div>',
};

export const TableStub: Component = {
  name: 'ElTable',
  props: { data: { type: Array, default: () => [] }, loading: Boolean },
  // 真实 el-table 在 `highlight-current-row` 下点行会同时发 `row-click` 与 `current-change`
  emits: ['row-click', 'current-change'],
  setup(props, { slots, emit }) {
    return () => {
      const rows = (props.data as unknown[]) ?? [];
      const children = h(
        'div',
        { class: 'table' },
        rows.map((row) =>
          h(
            RowProvider,
            {
              row: row as Record<string, unknown>,
              // 行点击透传给表格的 `@row-click` / `@current-change`（真实 el-table 的行为）
              onRowClick: (clicked: unknown) => {
                emit('row-click', clicked);
                emit('current-change', clicked);
              },
            },
            { default: () => slots.default?.() },
          ),
        ),
      );
      return children;
    };
  },
};

export const EmptyStub: Component = {
  name: 'ElEmpty',
  props: { description: { type: String, default: '' } },
  template: '<div class="empty">{{ description }}<slot /></div>',
};

export const CheckboxStub: Component = {
  name: 'ElRadio',
  props: { modelValue: { default: null }, value: { default: null } },
  emits: ['update:modelValue'],
  template: '<span><slot /></span>',
};


/**
 * el-switch 替身：渲染成原生 checkbox，这样"切换只看变更"能被真的触发。
 * `active-text` 也要渲染出来——那是用户唯一能看到的说明文字。
 */
/**
 * el-dialog 替身：渲染默认与 footer 插槽，并**暴露 `modelValue`** ——
 * 关闭状态是这类弹窗的关键行为（"点打开之后弹窗要关掉"），不可测就没法回归。
 */
export const DialogStub: Component = {
  name: 'ElDialog',
  props: {
    modelValue: { type: Boolean, default: false },
    title: { type: String, default: '' },
    width: { type: [String, Number], default: '' },
    /** 可拖动（弹窗会挡住后面的内容，得能拖开）。 */
    draggable: { type: Boolean, default: false },
  },
  emits: ['update:modelValue'],
  template:
    '<div class="el-dialog" v-if="modelValue"><div class="dialog-title">{{ title }}</div>' +
    '<slot /><slot name="footer" /></div>',
};

export const SwitchStub: Component = {
  name: 'ElSwitch',
  props: {
    modelValue: { default: false },
    activeText: { type: String, default: '' },
    size: { type: String, default: '' },
  },
  emits: ['update:modelValue'],
  template: `<label class="switch"><input type="checkbox" :checked="modelValue"
    @change="$emit('update:modelValue', $event.target.checked)" />{{ activeText }}</label>`,
};

/**
 * el-tree 替身：把 `data` 里的节点（含一层子节点）用默认插槽渲染出来，
 * 并显式提供 `:data` 作用域 —— 真实 el-tree 就是这么把节点数据交给插槽的，
 * 少了作用域，模板里的 `#default="{ data }"` 会在解构时直接炸。
 *
 * 展开/勾选这些行为交给真实 el-tree 与 E2E 覆盖，这里只保证"渲染与数据流转"能被单测。
 */
/**
 * 双击展开/收起用到的节点状态。
 *
 * 组件里走的是 el-tree 的内部 `store.nodesMap[path].expand()/collapse()`（公开 API 里
 * 没有"按路径展开"），所以替身也必须提供同一张表，否则这条交互在单测里完全测不到。
 * 状态放在模块级的 Map 里，跨多次 computed 求值保持稳定（否则每次读都是新对象，
 * "先展开、再双击应收起"这类断言就没法写）。
 */
interface StubNode {
  expanded: boolean;
  calls: string[];
}
const stubNodes = new Map<string, StubNode>();

/** 重置替身节点状态（测试 beforeEach 调用）。 */
export function resetTreeStub(): void {
  stubNodes.clear();
}

/** 某节点上发生过的 expand / collapse 调用序列。 */
export function treeStubCalls(path: string): string[] {
  return stubNodes.get(path)?.calls ?? [];
}

/** 直接把节点置为"已展开"，用于测"双击应收起"。 */
export function setTreeStubExpanded(path: string, expanded: boolean): void {
  const n = stubNodes.get(path) ?? { expanded, calls: [] };
  n.expanded = expanded;
  stubNodes.set(path, n);
}

export const TreeStub: Component = {
  name: 'ElTree',
  props: {
    data: { type: Array, default: () => [] },
    nodeKey: { type: String, default: 'path' },
    defaultExpandedKeys: { type: Array, default: () => [] },
    defaultCheckedKeys: { type: Array, default: () => [] },
    // 与真实 el-tree 对齐的默认值：`checkOnClickLeaf` 真实默认就是 **true**，
    // 所以"点文件标签顺带切换勾选"是真实行为 —— 测试里要能断言我们显式关掉了它
    checkOnClickLeaf: { type: Boolean, default: true },
    checkOnClickNode: { type: Boolean, default: false },
    expandOnClickNode: { type: Boolean, default: true },
  },
  emits: ['check', 'current-change', 'node-click', 'node-contextmenu'],
  computed: {
    /** 与真实 el-tree 一致：`store.nodesMap[path]` → 节点实例。 */
    store(): { nodesMap: Record<string, unknown> } {
      const map: Record<string, unknown> = {};
      const entry = (path: string): StubNode => {
        if (!stubNodes.has(path)) stubNodes.set(path, { expanded: false, calls: [] });
        return stubNodes.get(path)!;
      };
      const walk = (list: Array<Record<string, unknown>>): void => {
        for (const n of list) {
          const path = n['path'] as string;
          map[path] = {
            get expanded(): boolean {
              return entry(path).expanded;
            },
            expand(): void {
              const e = entry(path);
              e.expanded = true;
              e.calls.push('expand');
            },
            collapse(): void {
              const e = entry(path);
              e.expanded = false;
              e.calls.push('collapse');
            },
          };
          const children = n['children'] as Array<Record<string, unknown>> | undefined;
          if (Array.isArray(children)) walk(children);
        }
      };
      walk((this as unknown as { data: Array<Record<string, unknown>> }).data);
      return { nodesMap: map };
    },
  },
  methods: {
    getCheckedKeys(): string[] {
      return (this as unknown as { defaultCheckedKeys: string[] }).defaultCheckedKeys;
    },
    getCurrentKey(): string | null {
      return null;
    },
    setCheckedKeys(): void {},
    setCurrentKey(): void {},
    filter(): void {},
  },
  template: `<div class="tree">
    <template v-for="n in data" :key="n.path">
      <div class="tree-node" :data-path="n.path"><slot :data="n" /></div>
      <div v-for="c in (n.children || [])" :key="c.path" class="tree-node child" :data-path="c.path">
        <slot :data="c" />
      </div>
    </template>
  </div>`,
};

/** 通用替身集合：视图里用到的组件都能被吃掉，不产生 "Failed to resolve component"。 */
export const commonStubs = {
  'el-container': slotStub('ElContainer'),
  'el-aside': slotStub('ElAside'),
  'el-header': slotStub('ElHeader'),
  'el-main': slotStub('ElMain'),
  'el-card': slotStub('ElCard'),
  'el-form': slotStub('ElForm'),
  'el-form-item': slotStub('ElFormItem'),
  'el-dialog': DialogStub,
  'el-drawer': slotStub('ElDrawer'),
  'el-alert': AlertStub,
  'el-empty': EmptyStub,
  'el-menu': slotStub('ElMenu'),
  'el-menu-item': slotStub('ElMenuItem'),
  'el-radio-group': slotStub('ElRadioGroup'),
  'el-radio': CheckboxStub,
  'el-tag': slotStub('ElTag'),
  'el-badge': slotStub('ElBadge'),
  'el-breadcrumb': slotStub('ElBreadcrumb'),
  'el-breadcrumb-item': slotStub('ElBreadcrumbItem'),
  'el-descriptions': slotStub('ElDescriptions'),
  'el-descriptions-item': slotStub('ElDescriptionsItem'),
  'el-link': slotStub('ElLink'),
  'el-input-number': InputNumberStub,
  'el-icon': slotStub('ElIcon'),
  'el-progress': { name: 'ElProgress', props: { percentage: Number }, template: '<div class="bar" />' },
  'el-table': TableStub,
  'el-table-column': TableColumnStub,
  'el-input': InputStub,
  'el-button': ButtonStub,
  'el-switch': SwitchStub,
  'el-tree': TreeStub,
  'el-divider': opaqueStub('ElDivider'),
};
