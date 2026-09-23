import { createPinia } from 'pinia';
import { createApp } from 'vue';
import ElementPlus from 'element-plus';
import zhCn from 'element-plus/es/locale/lang/zh-cn';

// ⚠️ 必须显式引样式：`import ElementPlus from 'element-plus'` 只带组件不带 CSS，
// 少了这一行界面能跑但完全没有样式（图标会撑成巨大的黑色色块），
// 而组件级单测因为把 el-* 全替身掉了，是发现不了的——只有真实浏览器截图能看出来。
import 'element-plus/dist/index.css';

import App from './App.vue';
import './styles.css';

createApp(App).use(createPinia()).use(ElementPlus, { locale: zhCn }).mount('#app');
