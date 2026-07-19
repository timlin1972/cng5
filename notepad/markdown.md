# 一级标题

## 二级标题

### 三级标题

#### 四级标题

##### 五级标题

###### 六级标题

---

**粗體字**

***斜體兼粗體***

~~刪除線~~

++底線++

==螢光標記==

> 引用他人的文字

- 文本一
- 文本二
- 文本三
  或

* 文本一
* 文本二
* 文本三

1. 文本一
2. 文本二
3. 文本三

[Ameba官方論壇](https://forum.amebaiot.com/)

![RTL8722DM|690x460, 50%](upload://wDnARafH3W7KoN2DXayporADqLN.jpeg)

| Tables |  Amount Demo |
| :----: | -----------: |
| Col 1  | 1000<br>test |
| Col 2  |         1500 |

bef
<br>
af

`Test`

```rust=
impl Plugin for DevicePlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["list", "status", "poweron <target>", "poweroff <target>"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "list" => self.list(out),
            "status" => self.status(out),
            "poweron" => self.poweron(args.first().context("poweron 需要一個目標參數")?, out),
            "poweroff" => self.poweroff(args.first().context("poweroff 需要一個目標參數")?, out),
            other => bail!("device 不認得指令: {other}"),
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
```

- [ ] uncheck
- [x] check