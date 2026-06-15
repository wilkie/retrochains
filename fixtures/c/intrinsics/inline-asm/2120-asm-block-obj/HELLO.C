int main(void) {
  int x = 10;
  asm {
    mov ax, x
    add ax, 5
    mov x, ax
  }
  return x;
}
