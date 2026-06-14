int get(int i) {
  static int data[4] = {2, 4, 6, 8};
  data[i]--;
  return data[i];
}
int main(void) { return get(0) + get(3); }
