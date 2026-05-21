typedef int IntArr5[5];
int main(void) {
  static IntArr5 a = {1, 2, 3, 4, 5};
  return a[0] + a[4];
}
