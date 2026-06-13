int five(void) {
  asm mov ax, 5;
  return _AX;
}
int main(void) {
  return five();
}
