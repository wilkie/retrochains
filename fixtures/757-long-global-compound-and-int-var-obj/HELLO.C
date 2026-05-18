long g;
int main() {
  int x;
  g = 0xFF00;
  x = 0x0FF0;
  g &= x;
  return 0;
}
