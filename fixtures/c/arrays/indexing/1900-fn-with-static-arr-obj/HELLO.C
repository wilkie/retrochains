int lookup(int i) {
  static int table[5] = {10, 20, 30, 40, 50};
  return table[i];
}
int main(void) {
  return lookup(0) + lookup(2) + lookup(4);
}
