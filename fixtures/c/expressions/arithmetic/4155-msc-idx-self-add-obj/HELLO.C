int get(int i) {
  static int data[3] = {7, 11, 13};
  return data[i] + data[i];
}
