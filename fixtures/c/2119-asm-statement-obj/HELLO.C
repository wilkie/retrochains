int main(void) {
  int x = 5;
  asm mov ax, x;
  asm add ax, 1;
  return x + 1;
}
