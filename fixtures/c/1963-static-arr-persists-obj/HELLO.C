int get(int i) {
  static int data[3] = {7, 11, 13};
  data[i]++;
  return data[i];
}
int main(void) {
  return get(0) + get(0) + get(1);
}
