struct Vec {
  int arr[3];
};
struct Vec v = {{10, 20, 30}};
int main(void) {
  return v.arr[0] + v.arr[1] + v.arr[2];
}
